### Added

- `fs_xfs::salvage` reads the superblocks, allocation group headers and log of a damaged volume without mounting it: each structure is decoded past a failed checksum or a wrong identity where its bytes allow, every failure is returned as a finding with its kind, severity and byte range, and the volume's geometry is chosen from the copies that agree, scanning for them when the primary is unreadable. Nothing is written.
- `Superblock::parse_lenient`, `Superblock::checksum_matches`, `Superblock::geometry_differences`, `Superblock::ag_offset`, `Agf::decode` and `Agi::decode` expose the pieces those reads are built from.
