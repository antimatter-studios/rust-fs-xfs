### Added

- `fs_xfs::layers::XfsVolume` implements rust-fs-core's `VolumeAPI`, `OpenVolume`, `PrimitiveAPI` and `FileAPI`, so a caller reads an XFS volume the way it reads every other rust-fs driver. `open` mounts strictly, replaying a dirty log in memory; `open_for_salvage` opens whatever survives.
- `fs_xfs::layers::XfsSalvage` implements the shared salvage verbs over inode records: find candidates, walk extents, rebuild the tree, report damage, read by extent and materialize. Walks carry file offsets, so a sparse file rebuilds in place through `ExtentWalk::plan()`, and every parent claim carries its directory entry's name.

### Changed

- `fs_xfs::salvage` speaks rust-fs-core's evidence types (`Extent`, `Damage`, `DamageKind`, `Severity`, `Integrity`, `WalkEnd`).
- rust-fs-core moves to =0.5.1 from crates.io with its `layers` feature, and serde is a dependency, for the candidate detail.
