### Added

- `fs_xfs::salvage::find_inodes` scans any byte range for inode records, live, free and damaged, each with its checksum result and its damage, and `read_inode` reads one by number without a mount.
- `fs_xfs::salvage::walk_extents` follows an inode's data fork (inline data, an extent list, or a block-map B+tree) as far as it can be followed: a map block that fails only its checksum is used and reported, one that belongs elsewhere is reported and skipped, and the walk says why it ended. `read_mapped` reads the file through that map.
- `Inode::decode_unverified` decodes an inode core without its checksum, identity and structural checks.
