### Added

- `fs_xfs::salvage::inode_chunks` and `free_extents` walk an allocation group's inode and free-space B+trees without a mount, using a block that fails only its checksum and reporting and skipping one that belongs elsewhere, so the records of every intact leaf are kept.
- `fs_xfs::salvage::find_inodes_in_chunks` scans only the inode chunks a tree names, far less to read than a scan of the whole device.
