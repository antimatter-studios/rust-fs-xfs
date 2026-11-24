## Interfaces

| rust-fs-core's three layers (`fs_xfs::layers`): `XfsVolume` implements `VolumeAPI`, `OpenVolume`, `PrimitiveAPI` and `FileAPI`, and `XfsSalvage` the `SalvageAdapter` salvage verbs, built on `fs_xfs::salvage`; deleted extents and cluster steps named as missing; nothing written | Experimental | Unreleased (#624) | | `layers_conformance.rs` |
