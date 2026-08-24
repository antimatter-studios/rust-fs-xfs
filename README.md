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
| Inodes | not yet |
| Directories | not yet |
| Extents / bmbt | not yet |
| Extended attributes | not yet |
| Log replay | not yet — a dirty volume is refused, not silently misread |
| Write path | not yet |

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
./scripts/vm.sh up               # boot (first run provisions)
./scripts/vm-build-fixtures.sh   # build the geometry matrix into .vm-share/
cargo test --test oracle_vm_fixtures -- --nocapture
```

The comparison runs on the host, so the VM is only needed when fixtures are
regenerated — not on every `cargo test`. In CI no VM is involved at all: GitHub's Linux
runners are already Linux and build the same matrix natively.

Prerequisites: `brew install qemu virtiofsd`, plus
`vagrant plugin install vagrant-qemu-christhomas vagrant-notify-forwarder-christhomas`.

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
