### Fixed

- **A btree that cannot be walked leaves its group unchecked, not lost.**
  When a group's free-space, inode, reverse-mapping, refcount or free
  inode btree, its free list, or a record of its inode btree cannot be
  read, the check no longer reports that group's blocks as `lost`, no
  longer compares the free-space btrees, the AGF or the AGI with records
  it never read, and no longer compares a superblock counter that group
  could not add to. The scan is still reported partial, with
  `btree.unreadable`, `ag.agfl.unreadable` or `inobt.record` for the group.
