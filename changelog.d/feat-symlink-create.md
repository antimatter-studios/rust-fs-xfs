### Added

- **A symlink can be created (#462).** `Filesystem::create_symlink` and
  the C ABI's `fs_xfs_symlink(fs, target, linkpath)` make one through the
  log, as one create transaction. A target that fits the inode's literal
  area is kept inline; a longer one, up to the 1023 bytes XFS stores, goes
  in one run of blocks behind the v5 `XSLM` header, logged with the
  create. `tests/symlink_create_oracle.rs` has the kernel replay the log
  and read every target back, on 4 KiB and 1 KiB blocks, and
  `xfs_repair -n` find the volume clean.
