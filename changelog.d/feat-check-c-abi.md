### Added

- **The checker through the C ABI.** `fs_xfs_check` checks a mounted
  volume as `fsck.xfs` does, writing nothing: each finding goes to a
  callback with its code, severity, group, block, inode, field and words,
  and `fs_xfs_check_report_t` holds the totals, whether the scan was
  complete, whether the log held unapplied records, and whether the volume
  is clean. A C caller could not reach the checker before.
