### Added

- `fs_xfs::salvage::directory_entries` reads the names a directory holds without a mount, from a short-form directory or every data block its map reaches; a record that does not check out is reported and skipped, the read resuming at the next record boundary, and a block another directory owns is reported and not read.
- `fs_xfs::salvage::rebuild_tree` rebuilds parent claims among scanned inodes from those listings, so files whose directory is destroyed are kept as orphans rather than lost.
