### Added
- `fs.xfs mkdir --parents` (`-p`) creates missing directory chains on one writable mount and releases the volume clean. Existing directories succeed without writing; files, symlinks and invalid names are refused before creating any part of the chain.
