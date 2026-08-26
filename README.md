# rust-fs-xfs

Pure-Rust [XFS](https://docs.kernel.org/filesystems/xfs/index.html) driver over the
shared [`am-fs-core`](https://github.com/antimatter-studios/rust-fs-core) block-device
trait, exposing a stable C ABI (`fs_xfs_*`) for FFI from Swift, C, or Go.

## Byte order

**XFS stores every multi-byte on-disk field in big-endian order, on every host.** It is
the only big-endian format in this family — ext4, Btrfs and NTFS are all
little-endian — and it is the first thing to keep in mind when reading this code
alongside its siblings.

With one exception, which has already caused a bug here: **checksum fields are stored
little-endian**. The kernel's `xfs_end_cksum()` returns `~cpu_to_le32(crc)`, so a CRC
read big-endian like the rest of the structure makes every real filesystem look
corrupt.

## Status

| Area | Support |
|------|---------|
| On-disk version | v5 (primary target), v4 parsed |
| Superblock | geometry, feature masks, CRC32C, inode-number splitting |
| Allocation groups | AGF, AGI, with v5 self-describing identity checks |
| Free-space B+trees | both trees read and edited; extents freed and allocated |
| Inodes | v1/v2/v3 cores, `bigtime` and 64-bit extent counts |
| Directories | short form, block, leaf and node; rename within short form |
| Extents / bmbt | inline extent lists and the block-map B+tree |
| Symlinks | inline and remote (`XSLM`), across multiple extents |
| Extended attributes | on-disk shapes documented; reading not yet |
| Log replay | not yet — a dirty volume is refused, not silently misread |
| Log **writing** | inode cores, rename, truncate, allocating write |
| Write path | overwrite in place, plus the journalled operations above |

### What the write path can do

An overwrite of bytes that already exist touches no metadata, so it needs no journal and
is done directly. Everything else goes through the log, and each of these produces a
record the Linux kernel replays:

| operation | ops | items |
|---|---|---|
| rename within a short-form directory | 8 | 2 |
| truncate a file to nothing | 11 | 4 |
| write into an empty file, allocating | 12 | 4 |

Those op and item counts are not this driver's choice. They were measured from
filesystems the kernel wrote, recorded in `docs/transaction-shapes.md`, and the encoder
reproduced them without being fitted to them.

Nothing on disk is touched by a journalled operation — the record is the change. That is
what makes the result checkable: a filesystem that came out different is one something
replayed, and `xfs_repair -n` afterwards is what catches metadata that is plausible on
its own and inconsistent with the rest.

Each operation refuses by name what it cannot do rather than attempting it. See
`docs/transaction-shapes.md` for the list and for the shapes not yet written — create,
unlink and mkdir all need the AGI and the inode B+trees.

Features recognised in the superblock and gated rather than guessed: `finobt`,
`rmapbt`, `reflink`, `inobtcnt`, `ftype`, `sparse inodes`, `metadata UUID`, `bigtime`,
64-bit extent counters. An unknown *incompatible* feature bit is refused outright.

### Self-describing metadata

On v5, every metadata block carries a CRC32C plus the filesystem UUID, the allocation
group it belongs to, and a log sequence number. The checksum detects corrupted bits;
the identity fields detect a block that is internally *perfect* but landed in the wrong
place — a misdirected write, or a stale block left behind by an earlier filesystem on
the same device. This driver checks both on every header it parses, because a checksum
alone cannot catch the second case.

Note that the checksum covers the **whole sector**, not the structure — XFS hands the
full buffer length to its verifier, trailing zero padding included.

## Test contract

Two layers, and the second is the one that matters.

**1. Unit tests** (`src/`) parse structures built in-process. These prove the parser is
*self-consistent*. They cannot prove it is *correct*, because a misreading of the
format is baked into both the fixture and the parser, and they agree with each other
while disagreeing with the rest of the world.

**2. Cross-validation against real XFS tooling** (`tests/oracle_vm_fixtures.rs`).
Filesystems are built by the canonical `mkfs.xfs` and dumped with `xfs_db`; this driver
parses the same images and every field it reports must match the value `xfs_db` reports
for that field. Currently 9 geometries — 1k/2k/4k blocks, 512b/1k inodes, explicit
allocation-group counts, `reflink+rmapbt`, `bigtime`, and a v4 filesystem with no CRC
at all.

The second layer is not optional decoration. Three bugs were live in this crate with
the entire unit suite passing:

- the superblock magic had two bytes transposed,
- checksum fields were read big-endian in an otherwise big-endian format that stores
  *checksums* little-endian,
- the checksum was computed over the 264-byte structure instead of the whole sector.

Every one of them is invisible to a round-trip test and fatal against a real
filesystem. All three died on the first run against `xfs_db`.

```sh
cargo test                 # unit tests; green on a fresh clone
cargo test -- --ignored    # adds tests that shell out to xfsprogs (Linux)
```

### Generating fixtures

`mkfs.xfs` and `xfs_db` are Linux-only. On macOS an oracle VM supplies them — Debian
arm64 under QEMU, hardware-accelerated via HVF, so there is no emulation penalty:

```sh
./scripts/install-host-tools.sh      # what the VM needs on this machine
./scripts/vm.sh up                   # boot (first run provisions)
./scripts/vm-build-fixtures.sh       # the geometry matrix, for the superblock tests
./scripts/vm-build-log-fixtures.sh   # populated filesystems, for the log tests
./scripts/vm-build-stress-fixtures.sh  # trees built by a stress generator
cargo test --test oracle_vm_fixtures -- --nocapture
```

The fixture scripts build different things. `vm-build-fixtures.sh` formats a
filesystem per geometry and never mounts it, which is what the superblock and inode
parsers need. `vm-build-log-fixtures.sh` mounts, writes and unmounts, so the log keeps
the records that were written along the way — a log with nothing in it cannot disagree
with us about how an item is written.

`vm-build-stress-fixtures.sh` is the one whose contents nobody chose. It runs the two
stress generators from the filesystem test suite against a mounted filesystem and keeps
what they leave behind, with a manifest of every path generated inside Linux by the
kernel's own driver. The generators are built in the guest from a pinned release tag and
their binaries are only ever executed — see `tests/vagrant/debian/provision-stress-tools.sh`,
which records why.

The comparison runs on the host, so the VM is only needed when fixtures are
regenerated — not on every `cargo test`. In CI no VM is involved at all: GitHub's Linux
runners are already Linux and build the same matrix natively.

Prerequisites are installed by `./scripts/install-host-tools.sh`, which also says what
each is for. `vm.sh` checks them before every boot, so a missing one is reported with
the command that fixes it rather than as a failure to start.

Other `vm.sh` verbs: `run <cmd>`, `share`, `put <file>`, `down`, `destroy`.

## Building

```sh
cargo build --release
cargo clippy --all-targets -- -D warnings
```

Builds as both an `rlib` and a `staticlib`, so it links into a Rust dependency graph or
alongside sibling drivers in a C/Swift/Go consumer. Requires the sibling
`../rust-fs-core` checkout.

Install the git hooks once per clone:

```sh
./scripts/install-hooks.sh
```

## License

MIT — see [LICENSE](LICENSE).
