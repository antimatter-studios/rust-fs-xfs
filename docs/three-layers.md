# The three API layers

The rust-fs drivers share three layers of read API, so a program can treat
every format the same way:

| Layer | What it does |
|---|---|
| **Volume** | Open a device, read its geometry and superblock, and report its health. |
| **Primitive** | Read-only, damage-tolerant access to the on-disk structures, and the salvage verbs built on them. |
| **File** | Browse, read and copy files on a healthy volume. |

The traits for these layers live in `rust-fs-core`. This page maps what
this crate already has onto them, and lists what each layer still needs.
It is the plan for implementing them; the layers that exist are marked.

Nothing in any of the three layers writes to the device. The writable
surface (`Filesystem::mount_rw` and the `write`, `create`, `unlink` and
`repair` modules) stays this crate's own and is not part of them.

## Volume

| Need | This crate today |
|---|---|
| Open a device, read-only | `Filesystem::mount(Arc<dyn BlockRead>)`, `mount_with_cache`, `mount_with_realtime` |
| Recognise the format | `Superblock::parse` refuses a wrong magic with `Error::NotXfs` |
| Geometry | `Superblock`: `blocksize`, `sectsize`, `agcount`, `agblocks`, `dblocks`, `inodesize`, `dirblocksize()`, log placement |
| Superblock | `Filesystem::superblock()` |
| Health | `log::inspect` (`Empty`, `CleanlyUnmounted`, `NeedsReplay`), `Filesystem::was_replayed()`, and the full checker `check::check` |

A dirty log is replayed **in memory**: `Filesystem::mount` reads the log
with `log_recover::replay` into an `overlay::Overlay`, and every later read
goes through it. The device is never written. A log that needs replay and
carries `log_incompat` bits is refused rather than half-replayed.

Missing: a mount that does not need a valid primary superblock. Today a
volume whose primary superblock fails its CRC or its geometry checks
cannot be opened at all; the Volume layer of a damaged volume has to fall
back to a secondary copy.

## Primitive

These read the structures one at a time, without a mount, and do not stop
at the first bad checksum or broken link. Each result says what could be
read and what was wrong with it.

| Structure | This crate today | Damage-tolerant form needed |
|---|---|---|
| Superblock, every AG copy | `Superblock::parse`, `parse_copy` (strict: CRC, geometry, feature bits) | Read each copy, record CRC and geometry failures as damage, and keep the copy |
| AGF, AGI, AGFL | `ag::Agf::parse`, `ag::Agi::parse` (strict), `Filesystem::read_agf`/`read_agi` | Raw header plus integrity and identity (UUID, AG number) checks as findings |
| Free-space B+trees | `alloc_btree::walk_from_agf`, `Filesystem::free_extents` | Walk that keeps records from blocks it can read and reports the rest |
| Inode B+trees | `inode_btree::walk_from_agi`, `inode_btree::Trees` | As above |
| Inodes | `Inode::parse` (strict CRC and identity), `Filesystem::read_inode_raw` | An inode scan by magic over any byte range, with CRC and identity as integrity flags |
| Extent lists and BMBT | `extent::parse_list`, `bmbt::walk`, `bmbt::leaf_records_unverified` | An extent walk that keeps every extent reached and says why it stopped |
| Directories | `dir::parse_data_block`, `parse_block_form`, `parse_leaf`, `parse_node`, `read_short_form` | Entries from every readable directory block, with the owning inode where the block names one |
| Log | `log::inspect`, `log::head`, `log_recover::replay` | The log's state as a health finding |

### Salvage verbs

The salvage verbs are the format-neutral surface built on the primitives.
For XFS they mean:

| Verb | XFS meaning |
|---|---|
| Find candidates | Scan a byte range for inode records (magic `IN`), reporting live and free inodes, and CRC and identity failures as integrity, never dropping a record for them |
| Walk extents | Follow an inode's data fork: inline data, the extent list, or the BMBT, keeping every extent reached |
| Rebuild the tree | Parent claims from directory entries (short-form, block, leaf and node directories) and from `..`, keeping orphans and conflicting parents |
| Report damage | Superblock copies that disagree or fail their CRC, AG headers that fail their checks, and a log that needs replay |
| Read by extent | Bounded reads of byte ranges on the source |
| Materialize from a plan | Copy a file's extents, as a plan lists them, to a caller's sink |
| Capabilities | XFS frees an inode's extents before it frees the inode, so a deleted file's data is not mapped: deleted candidates are reported, and deleted extents are listed as missing |

Every verb takes the source as `&dyn BlockRead`, which has no write path.

## File

| Need | This crate today |
|---|---|
| Root and lookup | `Filesystem::root`, `lookup_path`, `open`, `open_ino` |
| List a directory | `Filesystem::read_dir`, `list_path` |
| Metadata | `Inode`: mode, size, times, link count, file type |
| Read | `Filesystem::read_at`, `read_file`, `read_path` |
| Symbolic links | `Filesystem::read_link` |

The File layer is a thin wrapper over what exists.

## Order of work

1. The damage-tolerant primitives and the salvage verbs, as this crate's
   own functions, each checked against `xfs_db` on images `mkfs.xfs` and
   the kernel made, and on damaged copies of them.
2. When `rust-fs-core` publishes the traits, implement them on top. The
   crate's own salvage functions stay for one release as deprecated
   wrappers.
