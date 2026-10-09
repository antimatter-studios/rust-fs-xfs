# Features

What this driver does today, what it refuses, and what is coming. **Every
pull request that adds, fixes, refuses or removes behaviour updates its row
here, in the same pull request** (AGENTS.md). The reasoning behind each change
is in [CHANGELOG.md](../CHANGELOG.md); the measured transaction shapes are in
[transaction-shapes.md](transaction-shapes.md).

**Since** is the release a row's current state shipped in, with the issue or
pull request the changelog cites for it. Work merged after the last release
is **Unreleased (#N)** until the next one. **Tracking** names the issue for
anything not finished.

States:

- **Supported**: works, and is checked against the reference tools or the
  Linux kernel in the harness VM.
- **Experimental**: works in every test, but is new.
- **Partial**: works for part of the case, and the row says which part.
- **Refused**: recognised and refused by name, rather than misread or
  approximated.
- **Not supported**: neither read nor refused by name.
- **Upcoming**: an open issue with a plan.

## Reading

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Superblock: geometry, feature masks, CRC32C, inode-number splitting | Supported | 0.1.0 | | `oracle_vm_fixtures.rs`, `oracle_mkfs.rs` |
| v5 (CRC, self-describing) volumes | Supported | 0.1.0 | #355 | `oracle_vm_fixtures.rs`, `endtoend_oracle.rs` |
| v4 volumes | Supported, read-only | 0.1.0 | | `oracle_vm_fixtures.rs` |
| v5 metadata identity: CRC, UUID, group, block number on every header | Supported | 0.1.0 | | `oracle_vm_fixtures.rs`, `dir_block_identity_oracle.rs` |
| Allocation groups: AGF, AGI, free list | Supported | 0.1.0 | | `oracle_vm_fixtures.rs`, `alloc_btree_oracle.rs` |
| Free-space and inode B+trees | Supported | 0.1.0 | | `alloc_btree_oracle.rs`, `inode_btree_oracle.rs` |
| Inodes: v1, v2, v3 cores, `bigtime`, 64-bit extent counts | Supported | 0.1.0 | | `oracle_vm_fixtures.rs`, `inode_version_oracle.rs` |
| An inode version the filesystem does not use | Refused | 0.11.0 (#92) | | `inode_version_oracle.rs` |
| Directories: short form, block, leaf and node | Supported | 0.1.0 | | `dir_oracle.rs`, `endtoend_oracle.rs` |
| Lookup through the directory's hash index | Supported | 0.8.0 | | `lookup_by_hash_oracle.rs` |
| Extent-format data forks | Supported | 0.1.0 | | `endtoend_oracle.rs` |
| B+tree-format data forks (bmbt), one and two levels | Supported | 0.4.0 | | `bmbt_two_level_oracle.rs`, `endtoend_oracle.rs` |
| Symlinks, inline and remote (`XSLM`) | Supported | 0.1.0; readlink contract 0.8.0 (#259) | | `capi.rs`, `endtoend_oracle.rs` |
| Extended attributes: short form, leaf, node, remote values | Supported | 0.8.0 (#91) | | `xattr_oracle.rs` |
| POSIX ACLs as `system.posix_acl_*`, in the VFS format | Supported | 0.10.0 (#285) | | `acl_oracle.rs`, `posix_acl_view_oracle.rs` |
| Dirty log: replayed into memory, the device untouched | Supported | 0.8.0 (#90) | | `dirty_log_mount.rs`, `log_oracle.rs` |
| Log head found from cycle numbers, not a whole-ring read | Supported | 0.9.0 (#251) | | `read_path_cost.rs` |
| `File` handle (`open`, `open_ino`, `root`) | Supported | 0.6.0 | | `file_handle_oracle.rs` |
| Parent pointers and exchange-range volumes | Supported, read-only | 0.8.0 (#99) | #373, #374 | `parent_exchrange_oracle.rs` |
| Realtime files, read from the realtime device | Supported | 0.10.0 (#98) | #361 | `realtime_oracle.rs` |
| A realtime file with no realtime device given | Refused (`RealtimeDeviceAbsent`, ENXIO) | 0.10.0 (#98) | | `realtime_oracle.rs` |
| Reverse-mapping and reference-count trees | Supported | 0.7.0 | | `rmap_oracle.rs`, `refcount_oracle.rs` |
| Filesystems shaped by the stress generators | Supported | 0.8.0 | | `stress_oracle.rs` |
| `finobt`, `rmapbt`, `reflink`, `inobtcnt`, `ftype`, sparse inodes, metadata UUID | Supported | 0.1.0 | #359 | `feature_matrix_oracle.rs` |
| An unknown incompatible feature bit | Refused | 0.1.0 | #360 | `src/superblock.rs` unit tests |
| Fuzzed parsers: superblock, group headers, inodes, directories, btrees | Supported | 0.8.0 (#96) | | `fuzz_decoders.rs` |

## Checking

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `fsck.xfs`, check-only: secondary superblocks, group headers against their btrees, block ownership, inodes, directory tree, link counts, counters | Supported | 0.11.0 (#339) | #356 | `cli_fsck_oracle.rs` |
| Findings with stable codes, severity and location, in a JSON report with `schema_version` 1 that tells a complete scan from a partial one or none | Experimental | Unreleased (#403) | | `cli_fsck_oracle.rs` |
| Quota records: quota inode references, record identities, v5 checksums and UUIDs, limits and grace timers, and accounting against allocated inode usage | Experimental | Unreleased (#402) | #378 | `quota_records_oracle.rs`, `oracle_vm_fixtures.rs` |
| Realtime metadata checked from the data device: geometry, each realtime file's extents, the bitmap against the extents files map, the summary against the bitmap, and `sb_frextents` (`rt.*`, `counter.sb.frextents`) | Experimental | Unreleased (#381) | | `realtime_check_oracle.rs` |
| Reverse-mapping btree checked block by block against the owners the walk finds: missing, stale, wrong-owner and duplicate records (`rmap.*`) | Experimental | Unreleased (#380) | | `rmap_check_oracle.rs`, `cli_fsck_oracle.rs` |
| Refcount btree checked against the file mappings that share each block: a record over unshared blocks (`refcount.stale`) and a wrong count (`refcount.count`) | Experimental | Unreleased (#379) | | `refcount_check_oracle.rs` |
| The checker's remaining gaps against `xfs_repair -n` | Upcoming | | #364 | |
| Repair planning, `fsck.xfs --dry-run` and `fs_xfs::repair`: proposed changes with before and after bytes, writing nothing; refused by `repair.*` finding without exclusive access, under `rmapbt`, realtime, quota or `log_incompat`, on a dirty log, an incomplete scan or ambiguous ownership | Experimental: no repair rule ships yet, so every error is listed as left | Unreleased (#404) | #376, #377 | `repair_plan.rs`, `cli_repair_plan_oracle.rs` |
| Repair (`-y`, `-p`) | Refused (exit 16) | 0.11.0 (#339) | #376, #377 | `cli_fsck_oracle.rs` |

## Writing

An overwrite of bytes that already exist changes no metadata and is written in
place. Everything else is a journalled record, and every record is replayed by
the Linux kernel and checked by `xfs_repair -n` in the guest. Writing is v5
only.

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| Overwrite existing bytes in place | Supported | 0.5.0 | #386 | `write_oracle.rs` |
| Change timestamps, permissions and ownership | Supported | 0.5.0; pre-1970 times 0.8.0 | | `capi.rs`, `write_oracle.rs` |
| Write into an empty file, allocating | Supported | 0.5.1 (#32) | | `file_write_replay_oracle.rs` |
| Write through holes and unwritten extents | Upcoming | | #387 | |
| Extents across allocation groups | Upcoming | | #388 | `crossag_oracle.rs` (freeing only) |
| Truncate to zero, including a B+tree data fork | Supported | 0.5.1 (#30); bmbt 0.8.0 (#222) | | `truncate_replay_oracle.rs`, `truncate_btree_fork.rs` |
| Shorten in place, without freeing blocks (C ABI `fs_xfs_truncate`) | Partial: the blocks stay allocated | 0.5.0 | #370 | `capi.rs` |
| Truncate to any length (`Filesystem::truncate_to`), shorter or longer, freeing the blocks past the new end, cutting a straddling extent, zeroing the last block's tail; an extent B+tree left small enough returned to an inline list; shared blocks kept, and a cut inside one refused | Experimental: a B+tree that would still need a tree after freeing is refused | Unreleased (#370) | | `truncate_to.rs`, `truncate_to_oracle.rs` |
| Create a file | Supported | 0.5.1 (#36) | | `create_replay_oracle.rs`, `create_free_slot_oracle.rs` |
| Create in a directory with a default ACL | Refused | 0.10.0 (#284) | #390 | `create_default_acl_oracle.rs` |
| Unlink a file | Supported | 0.5.1 (#37) | | `unlink_replay_oracle.rs` |
| mkdir, and a directory converted to block form | Supported | 0.5.1 (#38) | | `dir_block_oracle.rs` |
| Entries in a block-form directory | Supported | 0.8.0 (#215) | | `block_form_insert.rs` |
| Create, unlink, rename and rmdir in block- and leaf-form directories: the directory laid out again in block form while one block holds it and in leaf form beyond, blocks taken and given back as it grows and shrinks | Experimental | Unreleased (#366) | | `leaf_directories.rs`, `leaf_directories_oracle.rs`, `block_form_insert.rs` |
| Node-form directory changes | Refused by name | 0.8.0 (#215) | #367 | `leaf_directories.rs` |
| Rename within a short-form directory | Supported | 0.5.1 (#25) | #368 | `rename_oracle.rs` |
| Rename across directories, or over an existing target | Upcoming | | #382, #383 | |
| Remove an empty directory (`remove_directory`), with the parent's link count; a non-empty one refused (`DirectoryNotEmpty`, ENOTEMPTY) and a non-directory (`NotADirectory`) before anything is written | Experimental | Unreleased (#385) | | `remove_directory.rs`, `rmdir_replay_oracle.rs` |
| Hard links | Upcoming | | #384 | |
| Extended attribute and ACL writes | Upcoming | | #389, #390 | |
| Names no directory entry can hold | Refused | 0.8.0 (#192) | | `entry_names.rs` |
| Many journalled operations in one mount, each built on the ones before, including a crash at any checkpoint or record boundary replaying to a prefix of the sequence | Supported | 0.8.0 (#89); crash points Unreleased (#365) | | `many_ops_per_mount.rs`, `random_operations_replay.rs`, `checkpoint_boundary_crash_oracle.rs` |
| Log reuse in bounded memory (`sync`) | Supported | 0.8.0 (#89) | | `log_reuse.rs` |
| A checkpoint split across several log records | Supported | 0.8.0 (#216) | | `split_checkpoint.rs` |
| A checkpoint whose flush fails | Supported: the kernel replays to a consistent volume | 0.8.0 | | `torn_checkpoint_oracle.rs` |
| Writing after a failed log write or push | Refused (`Error::Io`, EIO): the mount reads and writes nothing more, as the kernel shuts down | Unreleased (#400) | | `log_write_failure.rs`, `torn_checkpoint_oracle.rs` |
| In-place writes after a mount has logged a change | Refused | 0.8.0 (#186) | | `in_place_after_checkpoint.rs` |
| Groups whose B+trees are more than one block deep | Supported | 0.7.0 | | `deeptree_oracle.rs` |
| Free-list refill | Supported | 0.8.0 (#197) | | `agfl_refill.rs` |
| Inode chunks on the inode alignment | Supported | 0.8.0 | | `inode_chunk_alignment.rs` |
| Shared extents: a partial free splits the reference-count record | Supported | 0.7.0 | | `refcount_oracle.rs` |
| Damaged group, inode-chunk, free-space and reference-count records | Refused before anything is built | 0.11.0 (#92, #314) | | `group_record_oracle.rs`, `free_space_record_oracle.rs`, `inode_chunk_record_oracle.rs`, `refcount_record_oracle.rs`, `extent_bounds_oracle.rs` |
| User, group and project quota accounting, with hard limits | Experimental | Unreleased (#396, #397) | #378 | `quota_accounting_oracle.rs` |
| Every write against every legal feature combination | Supported | 0.8.0 | #359 | `feature_matrix_oracle.rs` |
| Writing a volume with parent pointers or exchange-range | Refused | 0.8.0 (#99) | #373, #374 | `parent_exchrange_oracle.rs` |
| Writing a v4 volume | Partial: in-place overwrite only; journalled operations refused | 0.5.0 | | `feature_matrix_oracle.rs` |
| Writing a volume whose log is dirty | Refused | 0.5.0 | | |
| Superblock written back | Supported | 0.5.2 | | `super_write_oracle.rs` |
| Label (`set_label`) on every group's superblock | Supported | 0.11.0 (#341) | | `cli_label_kernel.rs` |

## Making a filesystem

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| `mkfs.xfs`: v5 with the standard formatter's defaults, byte-identical headers; 1, 2 and 4 KiB blocks, 300 MiB and up | Supported | 0.11.0 (#338) | | `mkfs_layout.rs`, `cli_mkfs_kernel.rs` |
| Any formatter option not listed in the README | Refused by name | 0.11.0 (#338) | | `cli_mkfs_kernel.rs` |
| Resize | Not supported (`not implemented`, exit 3) | 0.9.0 (#271) | | `tests/cli/test-get.sh` |

## Interfaces

| Feature | State | Since | Tracking | Checked by |
|---|---|---|---|---|
| C ABI: mount (path, callbacks, fs_core device, realtime), volume info, stat, directory iterator, read, readlink, write, truncate, set attributes | Supported | 0.3.0; realtime 0.10.0 (#291) | | `capi.rs`, `header_declares_every_entry_point.rs`, `header_sentinels_match_the_abi.rs` |
| In-image paths as bytes, not UTF-8 | Supported | 0.9.0 (#269) | | `capi.rs` |
| `fs.xfs` `ls`, `read`, `get`/`info` (`--features cli`) | Supported | 0.9.0 (#271) | | `cli_get_oracle.rs`, `cli_corrupt_oracle.rs`, `tests/cli/` |
| `fs.xfs` `write`, `mkdir`, `set label` | Supported, within the write shapes above | 0.9.0 (#271); label 0.11.0 (#341) | | `cli_write_kernel.rs`, `cli_label_kernel.rs` |
| `rust-fs-xfs doctor`, man pages, shell completions | Supported | 0.9.0 (#271) | | `cli_dispatch.rs`, `cli_docs.rs` |
