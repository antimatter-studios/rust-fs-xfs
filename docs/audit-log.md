# Write-path audit log

Every pass over this driver's write paths asks one question:

> Is there an edit that can be applied to a filesystem the driver has **misread**?

The question is not about a missing bounds check. It's about a value read one way and then used another, on an ordinary filesystem. That's the shape of every corruption defect found in this family of drivers so far. The same log, in the same format, is kept in `rust-fs-ext4`, `rust-fs-xfs`, `rust-fs-btrfs` and `rust-fs-ntfs`.

Each pass is one entry, newest first. An entry names:
- the commit it read;
- every module it looked at, with the result, "none found" included;
- each finding with its issue;
- what it didn't reach, so the next pass knows where to start.

## 2026-10-02, fifth pass: is every read a write depends on verified first?

Read at `c8dab2e` (#92). The earlier passes asked whether each edit's logic is right. This one asks, for every write entry point in the Rust API and the C ABI, whether each structure the write reads is verified before anything is written. "Verified" means the checks the kernel's own verifier makes for that structure: magic, CRC, identity (UUID, owner or `di_ino`, `blkno`), and range. The full per-entry-point table is in the #92 comment of the same date.

| Read | Result |
|---|---|
| `src/create.rs`: the free inode slot a create builds on | #310 |
| `src/fs.rs` / `src/extent.rs`: a data fork's extents (`data_extents`, `truncate_to_zero`'s B+tree branch) | #311 |
| `src/inode.rs`: `Inode::parse`'s version | #313 |
| `src/group_write.rs`, `src/agfl.rs`, `src/inode_btree.rs`: free-space, AGFL-entry and inode-chunk records | #314 |
| `src/superblock.rs`: `Superblock::parse` | none found |
| `src/ag.rs`: AGF and AGI | none found |
| `src/ag_btree.rs`: `parse_block` and the walk | none found |
| `src/bmbt.rs`: map blocks | none found |
| `src/create.rs` / `src/fs.rs`: directory blocks (`verify_dir_block`, since #290) | none found |
| `src/unlink.rs`: the victim against the inode tree (`give_back`) | none found |
| `src/dir_write.rs`: the moved inode | none found |
| `src/write.rs`: `update_inode` | none found beyond #313 |
| `src/capi.rs`: `fs_xfs_mount_rw`, `fs_xfs_write_file`, `fs_xfs_truncate`, `fs_xfs_set_attributes`, `fs_xfs_umount` | each re-reads the inode, so they inherit #311 and #313 and add nothing of their own |

**Findings:**
- **#310.** A create read the free slot straight off the device and kept its magic, version, `di_ino` and UUID. It checked none of them, nor the CRC, nor that `di_mode` was zero. So a stale or misread free bit handed out a live inode, and the create journalled a new file over it. All five damages were accepted: a live file, a bad CRC, a wrong `di_ino`, a wrong UUID and a wrong magic. `xfs_repair -n` reports each, e.g. `imap claims in-use inode 131 is free`.
- **#311.** No extent was checked against the geometry. `agblklog` rounds the group up to a power of two, so an extent past its group's length addressed the next group's headers.
  - `write_at` wrote 64 blocks of file data over AG 1's superblock, AGF, AGI and AGFL. The next mount failed with `AGI for ag 1 has magic 0xabababab`.
  - `truncate_to_zero` journalled a free of AG 1's headers and tree roots. The free-space overlap check could not stop it, because headers are never free.
- **#313.** `Inode::parse` took the version from the record, and checked CRC and identity only for version 3. On v5, one bit (3 to 2) switched those checks off and moved the data fork 76 bytes into the v3 core. `xfs_repair -n` reports `bad version number`.
- **#314, filed and not fixed.** Group-tree records are not checked as `xfs_alloc_check_irec`, `xfs_inobt_check_irec` and `xfs_agfl_verify` check them. `take` hands out the first free run long enough, an AGFL entry is not bounded, and `InodeChunk::take`'s `freecount -= 1` on a `u8` wraps in release. Every such record sits in a CRC-checked v5 block, so reaching one takes a block with wrong contents rather than a damaged one.

**Looked at and not a finding:**
- Every group B+tree block is checked for magic, CRC, UUID, owner, `blkno`, level against the parent, and `numrecs` before its records are read. The walk is bounded by the group's size.
- The AGF's roots and levels are not range-checked in `Agf::parse`. The walk refuses a block at the wrong address, owner or depth, so a misread root fails before anything is built.
- `unlink`'s `give_back` refuses a victim the inode tree already calls free. `rmap::remove` and the refcount release require exact matches.
- `sync` writes only the images the mount logged, CRC-stamped; it reads nothing from disk to decide.

**Still not reached:**
- `write_at` and `truncate` take the caller's `&Inode` and raw bytes and do not re-read them. `set_attributes` and the C ABI do re-read. Whether the Rust API should is a contract question, not a misread.
- `refcount::release`'s records, which are part of #314's class.
- The fourth pass's torn checkpoint: a record written and its flush then failing.

## 2026-09-30, fourth pass: block-form insert, many records per mount, the B+tree-fork free

Read at `0043d26` (#92). This pass covers the write paths that arrived after the third pass read `c6c0b26`, or that no pass had reached: adding an entry to a directory already in block form (#220), a mount that writes several checkpoints and reuses the ring (#205, #209), a checkpoint written as several records (#216), and freeing a B+tree extent fork's own blocks (#222).

| Module | Result |
|---|---|
| `src/create.rs`: `add_to_block_form` | #287 |
| `src/dir.rs` / `src/fs.rs`: every directory block read (`read_dir`, `lookup_by_hash`, `read_dir_block`) | #287 |
| `src/fs.rs`: `commit_record`, `sync`, the pad before a wrap, `logged_inode` | none found |
| `src/overlay.rs`: `push` | none found |
| `src/log_write.rs`: `split_into_records`, `blocks_for_records` | none found |
| `src/truncate.rs` with `bmbt::walk_with_blocks`: freeing a B+tree fork's own blocks | none found |
| `src/unlink.rs`: the short-form width after a removal | none found |

**Finding:**
- **#287.** A v5 directory block's CRC, `blkno`, UUID and owner are never checked on any path, because `dir::verify_data_block` and `verify_da_block` have no caller outside the tests. `add_to_block_form` reads the block, takes its entries and rebuilds it with `dir_block::build(.., parent, ..)`. That stamps this directory's owner and address on it, and recovery gives it a valid CRC. So a foreign or damaged block becomes a sound-looking block of this directory. Reproduced on a kernel-built volume by setting one block's `owner` to 999:
  - the kernel refuses it: `Structure needs cleaning`, `metadata I/O error ... error 74`;
  - `xfs_repair -n` reports `Metadata CRC error ... xfs_dir3_block`;
  - this driver lists all 30 entries, resolves a name through the hash index, and journals `create_file` into the block.

**Looked at and not a finding:**
- `commit_record` places a whole checkpoint before writing any of it. When it will not fit, it pushes first, pads to the end of the ring with an empty record in the old cycle, then wraps. So the tail never passes the checkpoint's own first record.
- `sync` clears `oldest_record` only after `Overlay::push` has written and flushed every buffer. The pad's tail is then its own LSN.
- `Overlay::push` writes images whose CRC `BufferItem::image_as_written` has already stamped, for every buffer type this driver logs: AGF, AGI, AGFL, the short-form AG B+tree blocks and the block-form directory block. It never writes a logged buffer's stale checksum in place.
  - A new chunk's 64 inodes go into the overlay as recovery would initialise them, so after a push and a wrap past its icreate record the chunk is still on disk.
- `logged_inode` starts from the full on-disk inode record and replaces only the core and the data fork. It keeps the attribute fork, as recovery does for an item without `XFS_ILOG_ADATA`.
- `split_into_records` divides only at operation boundaries. An operation larger than a record is refused by name. Examples are a fully dirty directory block on `-n size=65536` against a 32 KiB `h_size`. That is a refusal, not a torn record.
- Freeing a B+tree fork:
  - The map blocks come from `bmbt::walk_with_blocks`, and `parse_block` checks each one's CRC, owner, address and level before it is freed.
  - A block listed twice cannot be freed twice: `alloc_btree::free_extent` refuses an extent that overlaps free space.
  - `rmap::remove` requires an exact record, so a map that disagrees with the reverse map stops the free.
- `unlink`'s short-form rewrite recomputes `i8count` from the remaining inode numbers (#235). Removing the last wide entry narrows the directory, as `xfs_dir2_sf_toino4` does.

**Related, not a misread:** #284. A create in a directory with a default ACL does not inherit it, and it is filed separately.

**Still not reached:**
- Attribute writes: none exist, so there is nothing to audit.
- `src/dir_block.rs`'s leaf and node forms: no write path reaches them.
- The C ABI's write entry points (`src/capi.rs`), read only for how they map errors.
- What a failed device write part-way through a multi-record checkpoint leaves behind. `next_head` is not advanced, so the next checkpoint overwrites the partial one. If the new checkpoint is shorter, blocks of the uncommitted one remain past the new head. Recovery skips an uncommitted transaction. This was reasoned about but not probed.

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
