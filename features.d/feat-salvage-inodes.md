## Checking

| Salvage inode scan and extent walk (`fs_xfs::salvage::find_inodes`, `walk_extents`, `read_mapped`): inode records found by magic in any byte range, live, free and damaged, decoded past a failed checksum or identity; a data fork followed through inline data, an extent list or a block-map B+tree whose damaged blocks are reported and skipped, or used when only their checksum fails; the file read through the map, unreadable ranges listed; nothing written | Experimental | Unreleased (#616) | | `salvage_inodes.rs`, `salvage_inodes_oracle.rs` |
