# Write-path audit log

Every pass over this driver's write paths asks one question:

> Is there an edit that can be applied to a filesystem the driver has **misread**?

The question is not about a missing bounds check. It's about a value read one way and then used another, on an ordinary filesystem. That's the shape of every corruption defect found in this family of drivers so far. The same log, in the same format, is kept in `rust-fs-ext4`, `rust-fs-xfs`, `rust-fs-btrfs` and `rust-fs-ntfs`.

Each pass is one entry, newest first. An entry names:
- the commit it read;
- every module it looked at, with the result, "none found" included;
- each finding with its issue;
- what it didn't reach, so the next pass knows where to start.

## 2026-09-17, second pass: inode reuse, allocation, directory conversion

Read at `e8d3f4b` (#92), taking the modules the first pass listed as not reached.

| Module | Result |
|---|---|
| `src/unlink.rs`: the freed inode core | #189 |
| `src/create.rs`: the created inode core | #189 |
| `src/group_write.rs`: `GroupAlloc::open`, `take`, `give_back`, `release_shared`, `into_items` | none found |
| `src/unlink.rs`: free-inode tree membership (`give_back`, counts) | none found |
| `src/create.rs`: `convert_to_block_form` | none found |

**Finding:**
- **#189, fixed in #190.** A freed inode kept `di_flags`, `di_flags2` and its attribute fork, and a create read the flags back. So a file created in an inode this driver had removed inherited `IMMUTABLE`, `APPEND`, `REALTIME` or `REFLINK`. The kernel's `xfs_ifree` resets all of them. Pinned by unit tests against that rule.

**Looked at and not a finding:**
- `take` allocates one whole free run, records the rmap with its owner, and refuses a split. Tree growth goes through the AGFL.
- `convert_to_block_form` logs only the chunks of the new block that differ from zeros, so replay leaves a recycled block's old bytes in the unused regions. The v5 write verifier recomputes the CRC over the replayed buffer, so the block stays valid. Those bytes are slack, not structure.
- Per-AG reservations aren't honoured by `take`. Filling them can make the kernel warn at mount that the reservation failed; it doesn't corrupt anything.

**Still not reached:** `src/dir_write.rs` (rename), `src/super_write.rs`, and the B+tree `relay` layout in `src/ag_btree.rs`.

## 2026-09-17: write ordering and fencing (partial)

Read at `2d3d0f1` (#92). This pass covers how the write paths are ordered and fenced against each other, not every tree edit in depth.

| Module | Result |
|---|---|
| `src/write.rs`: `write_at`, `set_attributes`, `truncate` | #186 |
| `src/fs.rs`: `begin_checkpoint`, `check_log_is_clean` | #186 (the fence the in-place paths lacked) |
| `src/truncate.rs`: shared extents, rmap | none found |
| `src/file_write.rs`: ordering of data and record, allocation owner | none found |
| `src/inode_btree.rs`: choosing a free inode in a sparse chunk | none found |

**Finding:**
- **#186, fixed in #187.** A logged operation writes only its record, and the in-place writes weren't fenced by the mount's one checkpoint. After a logged `truncate_to_zero`, the disk still shows the file's extents. `write_at` reported success into blocks replay then frees, and `set_attributes` and `truncate` edits were put back by replay. Reproduced locally with `mkfs.xfs -p`.

**Looked at and not a finding:**
- `write_at` refuses holes, unwritten extents, reflinked, local and realtime inodes, and growth. The reflink flag is set on both inodes of a share.
- `truncate_to_zero` releases shared extents through the refcount tree (`release_shared`) and forgets their rmap records. It doesn't free them outright.
- `write_into_empty_file` writes the bytes before the record that claims them, so a crash in between loses space and never cross-links.
- `mount_rw` refuses a log that needs replay, and unlinked inodes.
- The sparse-chunk holemask is modelled, so a free bit inside a hole isn't handed out.

**Not reached, for the next pass:**
- `src/group_write.rs`'s B+tree edits (splits, AGFL use, per-AG reservations);
- `src/create.rs`'s short-form to block-form conversion;
- `src/unlink.rs`'s free-inode tree membership changes;
- `src/dir_write.rs`;
- `src/super_write.rs`.
