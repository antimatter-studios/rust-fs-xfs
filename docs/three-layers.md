# The three API layers

The rust-fs drivers share three layers of read API, so a program can treat
every format the same way:

| Layer | What it does | rust-fs-core trait | Here |
|---|---|---|---|
| **Volume** | Open a device, read its geometry and superblocks, report its health | `VolumeAPI`, `OpenVolume` | `layers::XfsVolume` |
| **Primitive** | Read-only, damage-tolerant access to the on-disk structures, and the salvage verbs | `PrimitiveAPI`, `SalvageAdapter` | `layers::XfsVolume`, `layers::XfsSalvage`, `salvage` |
| **File** | Browse, read and copy files on a healthy volume | `FileAPI` | `layers::XfsVolume` |

The traits and the evidence types (`Extent`, `Damage`, `Candidate`,
`ExtentWalk`, `Tree`, `Plan`) come from rust-fs-core's `layers` feature.
Only what is common to every format is there. The XFS structures stay in
this crate, in `fs_xfs::salvage`, and the traits are implemented on top of
them in `fs_xfs::layers`.

Nothing in any of the three layers writes to the device. Every source is a
`BlockRead`, which has no write path. The writable surface
(`Filesystem::mount_rw` and the `write`, `create`, `unlink` and `repair`
modules) stays this crate's own and is not part of the layers.

## Volume

| Trait method | XFS |
|---|---|
| `OpenVolume::probe` | The primary superblock's magic; `Damaged` when it is there and the sector does not check out |
| `OpenVolume::open` | The strict mount, `Filesystem::mount`: a dirty log is replayed in memory, never onto the source |
| `OpenVolume::open_for_salvage` | The superblock copies and the geometry chosen from them (`salvage::superblocks`), and the strict mount when it succeeds |
| `geometry` | Sector size, block size, `dblocks`, label and UUID, from the chosen geometry |
| `superblock`, `superblocks` | The chosen geometry; every copy with its integrity |
| `support` | Files when the strict mount succeeded; the salvage capabilities; a realtime device or an external log named as unsupported |
| `health` | `salvage::damage`: superblock copies, every group's AGF, AGI and AGFL, and the log. A damaged copy alone makes the volume `Degraded` |

## Primitive

The shared part is `structures` and the salvage verbs. The XFS primitives
under them are `fs_xfs::salvage`:

| Structure | `fs_xfs::salvage` |
|---|---|
| Superblock, every AG copy | `superblocks`, `read_superblock`, `scan_superblocks` |
| AGF, AGI, AGFL | `ag_headers` |
| Free-space and inode B+trees | `free_extents`, `inode_chunks`, `find_inodes_in_chunks` |
| Inodes | `find_inodes`, `read_inode` |
| Extent lists and block-map B+trees | `walk_extents`, `read_mapped` |
| Directories | `directory_entries`, `rebuild_tree` (with names) |
| Log | `log_health` |

Each one reads past a failed checksum or a wrong identity where the bytes
allow, and returns what it read with the damage beside it.

| Salvage verb | XFS |
|---|---|
| Find candidates | Inode records by magic in any byte range, live and free, each with its checksum and identity checked |
| Walk extents | The data fork: inline data, the extent list or the block-map B+tree, as file-ordered `WalkExtent`s with their file offsets; a gap is a hole and an unwritten extent is uninitialized, so `ExtentWalk::plan()` rebuilds a sparse file in place |
| Rebuild the tree | Parent claims from every live directory's entries, each with the entry's name as bytes, one claim per hard link. A claim from a directory that is not among the candidates is left out |
| Report damage | As `health` |
| Read by extent | Bounded reads of the source |
| Materialize from a plan | rust-fs-core's, over extent, zero and byte steps |
| Capabilities | Missing: deleted extents (XFS frees a file's blocks before its inode), cluster steps (no cluster chains), encoded extents (no compression) and geometry hypotheses (the superblock copies and a scan stand in for one) |

## File

| Trait method | XFS |
|---|---|
| `root`, `read_dir`, `metadata` | The strict mount's root inode, directory reads and inode cores; a node is an inode number |
| `read_at`, `read_link` | `Filesystem::read_at`, `Filesystem::read_link` |
| `names_match` | Exact, or ASCII case-insensitive on a volume made with `-n version=ci`; `DirEntry::bytes` holds the stored name, so a name that is not UTF-8 is looked up by `lookup_bytes` |

## Checked by

- `tests/layers_conformance.rs`: rust-fs-core's `check_healthy` on every
  fixture, `check_damaged` and the salvage set (`conformance::salvage::run`)
  on damaged copies, and every file of the data fixtures read through the
  File layer to the size and SHA-256 Linux gave it.
- `tests/salvage_*.rs` and `tests/salvage_*_oracle.rs`: the XFS primitives
  against `xfs_db`, `xfs_repair -n` and the kernel's manifests.

## What was not moved

`Filesystem` and the rest of the crate's own API stay as they were: they are
the strict read and write surface the layers are built on, not an API the
layers replace, so nothing is deprecated.
