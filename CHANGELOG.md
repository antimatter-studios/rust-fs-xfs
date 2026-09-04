# Changelog

Notable changes to `am-fs-xfs`, newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

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
