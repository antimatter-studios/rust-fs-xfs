### Changed

- `fs_xfs::salvage::read_mapped` retries a failed read a sector at a time, so a bad sector or the end of a truncated image loses only the sectors that do not read, not the whole piece of the file.
