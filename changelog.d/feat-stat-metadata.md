### Added

- `fs.xfs stat PATH` reports identity, type, permissions, owner, links, size,
  allocated bytes, device numbers, symlink targets and every timestamp to the
  nanosecond. XFS inode metadata is nested under `xfs`, unknown flag bits are
  retained as hex, and v4 creation times are null. Symlinks are reported without
  following them; paths retain their raw byte handling (#517).
