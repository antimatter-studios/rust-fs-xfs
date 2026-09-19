# Write-path audit log

Every pass over this driver's write paths asks one question:

> Is there an edit that can be applied to a filesystem the driver has **misread**?

The question is not about a missing bounds check. It's about a value read one way and then used another, on an ordinary filesystem. That's the shape of every corruption defect found in this family of drivers so far. The same log, in the same format, is kept in `rust-fs-ext4`, `rust-fs-xfs`, `rust-fs-btrfs` and `rust-fs-ntfs`.

Each pass is one entry, newest first. An entry names:
- the commit it read;
- every module it looked at, with the result, "none found" included;
- each finding with its issue;
- what it didn't reach, so the next pass knows where to start.

## 2026-09-19, third pass: rename, the superblock writer, and relaid group trees

Read at `c6c0b26` (#92), taking the three modules the second pass listed as not reached.

| Module | Result |
|---|---|
| `src/dir_write.rs`: `rename_in_directory`, `encode_short_form`, cookies | #235 |
| `src/super_write.rs`: `apply`, `stamp_crc` | none found |
| `src/ag_btree.rs`: `relay`, `build`, `stamp` | none found |
| `src/log_recover.rs`: replaying into memory (#90, new this pass) | none found |

**Finding:**
- **#235.** `encode_short_form` takes the directory's inode-number width from the header it parsed — `let wide = parsed.i8count != 0` — and a create adds a *new* inode number to that list. Nothing checks it fits. A probe against the encoder with a narrow header asked for inode `8589934592` and stored **0**: the entry names an inode that does not exist, silently. An inode number is `agno << (agblklog + inopblog) | agino`, so the fifth allocation group of a 640 GiB volume is already above 2^32 — an ordinary filesystem, not a hostile image. The kernel converts the fork instead (`xfs_dir2_sf_toino8`).

**Looked at and not a finding:**
- `rename_in_directory` refuses a v4 filesystem, a name `xfs_dir2_namecheck` would reject, a directory past short form, a name already taken and a name that is not there, before anything is built. The replacement entry gets a fresh cookie, and `encode_short_form` refuses a cookie past the two-byte field rather than truncating it into a collision — the same class as the finding above, handled.
- The moved inode's core is logged with only its changecount bumped, read through the mount's overlay rather than from the disk, so a rename after another logged operation sees that operation.
- `super_write::apply` gates every v5 field on `is_v5`, writes `sb_meta_uuid` only when the incompat bit is set (the parser reports the ordinary UUID there when it is not, and writing that back would change the checksum), and fills `sb_fname` as a fixed 12-byte field rather than a C string. **No write path calls it**: its only callers are `tests/super_write_oracle.rs` and `tests/fs_refusals.rs`, so there is no edit it can apply to anything.
- `ag_btree::stamp` writes both sibling pointers as `NULLAGBLOCK` on every block of a relaid tree, including leaves that have neighbours. Measured rather than reasoned about, because the kernel's own cursor reads those pointers:
  - `xfsdeep-bno2.img` relaid by this driver has a level-1 root over **three** leaves of 101, 101 and 100 records, every one of them `leftsib = null, rightsib = null`;
  - `xfs_repair -n` calls the volume sound;
  - the kernel mounts it and fills it to **333,888 KiB of 334,801 KiB available — byte for byte what the unmodified fixture accepts**. So the allocator finds every free extent in every leaf. `xfs_btree_increment` climbs to the parent and descends again when a leaf has no right sibling, which is why.
- `relay` reads a block's previous content through the mount's device — the overlay on a read-write mount — so the chunks it logs are the difference from what recovery will have produced by the time this record is replayed, not from a disk that is behind.
- `log_recover` applies into an `Overlay` and writes nothing to the device; `mount_rw` still refuses a log that needs replay, so no edit can be built on a replayed view.

**Still not reached:**
- `src/attr.rs` and the attribute fork's write paths;
- `src/dir_block.rs`'s leaf and node forms, which no write path reaches yet;
- the interaction between a relaid group tree and the AGFL blocks `relay` hands back, under a mount that then allocates.

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
