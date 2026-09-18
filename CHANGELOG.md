# Changelog

Notable changes to `am-fs-xfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Added

- **A directory takes entries after it has outgrown the inode.** A
  short-form directory that would not hold one more name was moved into a
  block of its own, and the next entry was then refused: "has outgrown the
  inode, so adding an entry rewrites a directory block rather than the
  inode's own fork". A directory therefore held about two dozen names and no
  more. A create into a directory already in block form now lays that block
  out again with the entry in it, through the same `dir_block::build` the
  conversion uses, and logs the block. A 4 KiB block holds about 124 names
  of 10 characters; past that the create is refused naming leaf form, which
  is not implemented (#215).
- **A mount keeps going, in bounded memory.** Every buffer a record carried
  was held for as long as the mount ran and the log was never reused, so the
  memory grew with the run and the 32,751st operation was refused with
  "only 2 remain before the log wraps". `Filesystem::sync` now writes those
  buffers where they belong, flushes and lets go of them — the push an XFS
  mount makes through the AIL — and it runs when 16 MiB is held, when a
  record will not fit in what is left of the ring, and whenever a caller
  asks. The ring is then started again from its beginning in the next cycle,
  with the blocks left at its end filled by an empty record, because a
  reader walks the cycle number in every block and a gap reads as a corrupt
  header. `log_wraps` and `dirty_bytes` report both. The head is remembered
  between records rather than found by scanning the ring, which was 52 ms an
  operation (#89).
- **A mount writes as many journalled operations as it likes.** A record
  changes nothing on disk, so the second operation of a mount read the state
  the first started from — two creates handed out one inode — and the mount
  refused it rather than write it. Every buffer a record carries now goes
  into an overlay that a writable mount reads through, so each operation is
  built on what its predecessors logged, which is what a replay arrives at.
  What goes in is what recovery writes: buffer images with the checksum
  recovery computes, inode cores with their fork and CRC, and every inode of
  a chunk an `icreate` names. `h_tail_lsn` names the mount's first
  outstanding record rather than the record itself, because a tail past a
  record recovery still needs loses a committed transaction. In-place writes
  are still refused once anything has been logged (#89).
- **Volumes with parent pointers or exchange-range can be read.** Both
  incompat bits are accepted by a read-only mount. `mkfs.xfs -n parent=1`
  sets both, and `-i exchange=1` sets exchange-range alone. Parent pointers
  are attributes in their own namespace, which `list_xattrs` already leaves
  out. `mount_rw` refuses either bit, saying the volume can be read but not
  written, because no write here maintains parent pointers.
  `tests/parent_exchrange_oracle.rs` builds both kinds of volume with a
  pinned xfsprogs 6.13 (`scripts/build-xfsprogs.sh`), which CI builds and
  caches (#99).
- **Extended attributes can be read.** `Filesystem::list_xattrs` and
  `get_xattr` read an inode's attribute fork in every shape: short form
  inline in the inode, a single leaf block, a node B-tree over chained
  leaves, and remote values over several blocks (each block's v5 header
  stripped). Names come back with their `user.`, `trusted.` or `security.`
  prefix; entries still marked incomplete are not returned.

### Changed

- **A lookup goes through the directory's hash index.** `Filesystem::lookup`
  listed the whole directory to find one name, reading every data block of
  it for every path component. Block-, leaf- and node-form directories are
  now searched by name hash (the block's tail index, the leaf block, or the
  node B-tree down to a leaf), and only the data blocks the matching
  records point at are read: one lookup in a 20,000-name directory makes 6
  device reads where the listing made 122. Short-form directories, which
  live in the inode, are scanned as before.
- **BREAKING (C ABI).** `fs_xfs_set_attributes` and `fs_xfs_truncate`
  take a new sentinel for timestamps: `FS_XFS_LEAVE_TIME`
  (`INT64_MIN`), not `FS_XFS_LEAVE` (`-1`). A caller that passed `-1`
  to leave `atime_sec` or `mtime_sec` alone will now SET that timestamp
  to one second before the epoch. There is no version of this fix that
  is not a break: `-1` is a real date, 1969-12-31T23:59:59Z, and making
  it reachable is the point. Every negative timestamp used to mean
  "leave this alone", so no date before 1970 could be set at all — the
  call returned 0 and the field did not move. `mode`, `uid` and `gid`
  keep `FS_XFS_LEAVE`, which is the `chown(2)` convention and correct
  for them.
  Callers should replace `FS_XFS_LEAVE` with `FS_XFS_LEAVE_TIME` in the
  `atime_sec` and `mtime_sec` positions, and in `fs_xfs_truncate`'s
  `mtime_sec`. Dates earlier than 1901-12-13 are accepted by the ABI
  and clamped by the on-disk encoding, which is unchanged.

### Fixed

- **A file whose extents live in a B+tree can be truncated.** Once a file has
  more extents than its inode holds, its map moves into a B+tree, and
  freeing such a file was refused: the tree's own blocks belong to the inode
  and would have been left allocated. `truncate_to_zero` now walks the fork
  with `bmbt::walk_with_blocks`, frees the data and the tree's blocks
  together — the latter recorded in the reverse map as `OFF_BMBT_BLOCK`, as
  the kernel records them — and puts the fork back as an empty extent list
  (#222).
- **A new inode chunk starts on the inode alignment.** A create that needed
  a new chunk took its blocks from the first free run long enough, wherever
  that run started. The kernel finds a chunk's inodes by masking their block
  down to `sb_inoalignmt`, so on 1 KiB blocks, where that is 32, a chunk at
  block 91 had its inodes replayed over the file data at block 80. The
  kernel refused the log (`xlog_recover_items_pass2`, error 117), and the
  volume would not mount. Chunks are now taken from an aligned start, by the
  kernel's rule in `xfs_ialloc_cluster_alignment`. On 4 KiB blocks the
  alignment is 8, and the chunk the tests used happened to start on it.
  `tests/inode_chunk_alignment.rs` runs creates on 1 KiB blocks, with the
  kernel replaying every step, until a second chunk is needed.
- **A group's free list is refilled.** Every block a group's B+trees grow
  into comes off its free list (AGFL), and the driver only ever took from
  it. On rmapbt, where a 1 KiB leaf holds 40 records, a few splits emptied
  the list, and from then on every write that needed a tree block was
  refused with `…free list is empty, and refilling it is not implemented`.
  Before a group's trees are laid out again, the list is now topped up out
  of free space to the kernel's `xfs_alloc_min_freelist`: twice the height
  of each free-space tree and of the reverse map. Refill blocks are recorded
  in the reverse map as `OWN_AG`. `tests/agfl_refill.rs` has the driver write
  500 one-block files on rmapbt, with the kernel replaying each; before the
  fix, write 432 was refused (#197).
- **A created file no longer inherits the flags of the file its inode last
  held.** `unlink_file` left `di_flags`, `di_flags2` and the attribute fork
  in the inode it freed, and `create_file` read them back, so a new file
  could come out immutable, append-only, real-time or reflinked. A freed
  inode is now reset the way the kernel's `xfs_ifree` resets it, and a
  created one starts with no flags beyond `BIGTIME` and `NREXT64` (#189).
- **In-place writes are refused once a mount has logged a change.** A
  logged operation writes only its record, so the disk is out of date until
  replay. `write_at`, `set_attributes` and `truncate` read that stale disk:
  a write after `truncate_to_zero` reported success into blocks replay then
  frees, and an attribute change was put back by replay. They now refuse
  after the mount's checkpoint, as a second logged operation already did
  (#186).
- **A rename or create refuses a name no directory entry can hold.**
  `rename_in_directory` checked only the new name's length, and logged
  names containing `/` or NUL, and `.` and `..`. `create_file` let NUL
  through, and a name longer than the one-byte length it is stored in.
  Both now apply the kernel's `xfs_dir2_namecheck` rule, before claiming
  the mount's checkpoint (#192).

## [0.7.0] — 2026-09-06

### Fixed

- Every write path works in a group whose B+trees are more than one
  block deep. There were 27 places that refused one, and a 4 KiB root
  holds 505 free-space records or 252 inode chunks, so any filesystem
  with real fragmentation or more than sixteen thousand inodes hit them.
- The reverse-mapping tree is read as the interval tree it is: a node
  entry carries the lowest key beneath it and the highest, so its
  pointer array starts twice as far into the block. Reading it as one
  key per entry took a pointer out of the middle of the key array, which
  came back as block zero -- the superblock. Only reachable at two
  levels or more.
- One operation allocates once from a group however many times it takes
  from it. A create that needed both a new inode chunk and a directory
  block read the group twice and handed out the same run twice.
- Freeing part of a shared extent splits the reference-count record
  rather than refusing, and records that adjoin and say the same thing
  are merged -- xfs_repair reads three records where one belongs as a
  reference count that is simply wrong.

### Added

- `ag_btree`, the one descent and one layout for a group's four
  short-form trees, and `agfl`, the group's free list.

## [0.6.0] — 2026-09-04

Minor rather than patch: `File` and the three constructors that return
it are new public API, and for a `0.x` crate the minor is the
compatibility boundary. Nothing existing changed — every low-level
`(&Inode, &[u8])` call keeps its signature.

### Added

- **`File`, and `Filesystem::open` / `open_ino` / `root`.** XFS threads
  the raw inode fork through every read, because an inode keeps its
  extents — and when small enough its data or directory entries —
  inside that fork. So the low-level calls take `(&Inode, &[u8])`, and a
  caller had to carry both values and remember they belong together.
  `File` carries them.

  It also removes a read. `lookup_path` walks the tree holding
  `(inode, raw)` at every component and returns **only the inode**, so
  anyone who then read the file fetched the last inode a second time.
  `read_path` and `list_path` did exactly that, in this crate. They
  delegate to `open` now.

  Measured, not asserted — `tests/file_handle_oracle.rs` wraps the
  device in a counter and compares the routes on a real `mkfs.xfs`
  image:

      /small.txt: handle 4 reads / 1548 bytes, low-level 5 reads / 2060 bytes

  `File` is a **snapshot**: nothing invalidates it, so one held across a
  write to the same inode is stale. Safe for open-read-drop within an
  operation; re-open otherwise, which is one inode read.

### Changed

- `lookup_path` is retained and now delegates to `open`, with docs
  saying why `open` is the one to reach for.


## [0.5.2] — 2026-09-04

### Added

- **A superblock can be written back, and is proved against `mkfs.xfs`.** Every
  field is modelled, so one can be built from nothing rather than only edited
  in place.

### Fixed

- **The block-map tree's block addresses are checked**, as the other two trees
  already were. An unchecked address is a read at an arbitrary offset.

### Changed

- Truncate now says which of its two paths journals and which frees the blocks.
  They are different operations with different crash behaviour and the names
  did not distinguish them.

## [0.5.1] — 2026-08-29

### Added

- **Directory writes: make a directory, build a block-form directory byte for
  byte as the kernel builds it, and convert a directory to block form** — the
  last of the measured transaction shapes.
- The build and the tests run through `chore`.

### Fixed

- **The mount's one checkpoint is spent on writes, not on refusals.** A refused
  operation was consuming it, so a later legitimate write had none left.
- The VM lock no longer skips tests quietly — a skipped test that looks like a
  passing one is worse than a failure.
- Test VMs are brought down when a fixture build finishes, teardown confirms
  the machine is actually down rather than assuming it, and a VM leaked by
  `lifecycle: after_all` is reaped.

### Changed

- Pinned toolchain moves to 1.95.0, in lockstep with the rest of the family.
- The CI lint gate can be run locally, and it is the same gate everywhere.

## [0.5.0] — 2026-08-26

### Added

- **The write path, exposed through the C ABI**: overwrite file data in place,
  change an inode's timestamps, permissions and ownership, and shorten a file.
  The inode-update path is shared rather than repeated per operation.

## [0.4.0] — 2026-08-25

### Added

- **B+tree-format data forks** are read, not just extent-format ones.

### Fixed

- **A dirty log is decided by reading the log**, rather than by a heuristic
  that could call a clean filesystem dirty or the reverse.

## [0.3.0] — 2026-08-25

### Added

- `fs_core` mounting, and a C ABI aligned with the sibling drivers so a host
  binds all of them the same way.

### Fixed

- `readlink` refuses a buffer too small for the target instead of truncating
  the path silently.

## [0.1.0] — 2026-08-25

### Added

- Initial release: a clean-room XFS reader — superblock, allocation groups,
  inodes, extents, and all four directory formats.
- **A blocking real-kernel validation gate in CI.** It found three parser bugs
  before the first release, which is the point of having it.
- Inode parsing is cross-validated against the reference XFS debugger.
- The C ABI, with its tests written alongside it.

[Unreleased]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.1.0...v0.3.0
[0.1.0]: https://github.com/antimatter-studios/rust-fs-xfs/releases/tag/v0.1.0
