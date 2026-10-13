### Added

- **`fs_xfs_attr_t` reports each time's nanoseconds (#465).**
  `atime_nsec`, `mtime_nsec`, `ctime_nsec` and `crtime_nsec`, appended to
  the struct, where a time set with nanoseconds through
  `fs_xfs_set_attributes` read back truncated to the second. The struct
  grows, so a caller rebuilds against the new header; 0.14.0.
