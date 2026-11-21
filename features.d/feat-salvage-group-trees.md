## Checking

| Salvage walks of a group's inode and free-space B+trees (`fs_xfs::salvage::inode_chunks`, `free_extents`, `find_inodes_in_chunks`): a block that fails only its checksum used and reported, one with the wrong magic, owner, address or level reported and skipped, cycles and unreadable blocks reported; inode scans narrowed to the chunks the tree names; nothing written | Experimental | Unreleased (#618) | | `salvage_groups.rs`, `salvage_groups_oracle.rs` |
