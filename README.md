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
| Inode B+trees | both trees read; inodes taken and given back |
| Inodes | v1/v2/v3 cores, `bigtime` and 64-bit extent counts |
| Directories | short form, block, leaf and node; rename within short form |
| Extents / bmbt | inline extent lists and the block-map B+tree |
| Symlinks | inline and remote (`XSLM`), across multiple extents |
| Extended attributes | read (`list_xattrs`, `get_xattr`): short form, leaf, node and remote values; not written, not in the C ABI |
| Log replay | read-only: a dirty volume is replayed into memory and reads as the kernel reads it; the device is not written |
| Log **writing** | inode cores, rename, truncate, allocating write, create, unlink, mkdir, directory conversion |
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
| create an empty file | 14 | 5 |
| remove an empty file | 14 | 5 |
| make an empty directory | 15 | 5 |
| convert a directory to block form | 23 | 9 |

Those op and item counts are not this driver's choice. They were measured from
filesystems the kernel wrote, recorded in `docs/transaction-shapes.md`, and the encoder
reproduced them without being fitted to them.

Nothing on disk is touched by a journalled operation — the record is the change. That is
what makes the result checkable: a filesystem that came out different is one something
replayed, and `xfs_repair -n` afterwards is what catches metadata that is plausible on
its own and inconsistent with the rest.

**A mount writes at most one checkpoint.** A journalled operation touches nothing on
disk, so a second would be built from a disk that does not yet reflect the first — two
creates in a row would hand out the same inode. The second attempt is refused rather
than answered wrongly; supporting more needs a dirty-block overlay this does not have.

That budget is spent by writing, not by asking. An operation this driver refuses — a
name that already exists, an inode of a shape it cannot rewrite — leaves the disk as it
found it and leaves the checkpoint for whatever the caller does next.

Each operation also refuses by name what it cannot do rather than attempting it. See
`docs/transaction-shapes.md` for the list. Every shape that document measured is now
written; what remains is the next size up — a directory that has outgrown a single
block, which is the leaf form.

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
for that field. Currently 11 geometries, listed once in `scripts/fixture-geometries.sh`
— 1k/2k/4k blocks, 512b/1k inodes, explicit allocation-group counts, `reflink+rmapbt`,
`bigtime`, a v4 filesystem with no CRC at all, and a v5 one with sparse inodes off.

The second layer is not optional decoration. Three bugs were live in this crate with
the entire unit suite passing:

- the superblock magic had two bytes transposed,
- checksum fields were read big-endian in an otherwise big-endian format that stores
  *checksums* little-endian,
- the checksum was computed over the 264-byte structure instead of the whole sector.

Every one of them is invisible to a round-trip test and fatal against a real
filesystem. All three died on the first run against `xfs_db`.

```sh
chore test:unit    # no tool, no fixture, no VM; green after `chore siblings`
chore test:images  # adds the tests that read a fixture, still without a VM
chore test         # everything, exactly as CI runs it
./scripts/test.sh --test oracle_vm_fixtures -- --nocapture   # one suite, by hand
```

There is no `--ignored` tier and no host-side oracle. Every test that runs a tool runs
it in the guest, and a test that cannot reach one **fails** rather than reporting `ok`
— see `## Generating fixtures` below.

`scripts/test.sh` and every tier go through `scripts/with-test-temp.sh`, which exports
an isolated `TMPDIR` and cleans the child it owns. That wrapper is not only tidiness:
the guest sees this repository and nothing else of the host, so a scratch directory
under `/tmp` is a path the tool asked to read an image cannot open. An exact
`FS_XFS_TEST_TMPDIR` is caller-managed; `FS_XFS_TEST_TMP_BASE` receives a unique
child; GitHub Actions uses `RUNNER_TEMP`; and Raspberry Pi uses this checkout's
`./tmp` so fixture churn follows the checkout onto NVMe rather than the system
SD card. Other hosts use `TMPDIR` or their platform temporary mechanism.

### Generating fixtures

**Every fixture is made by one kernel and one `mkfs.xfs` — the guest's — on a
developer's machine and on a CI runner alike.** A fixture is only evidence if the
thing that built it is the same everywhere: `mkfs.xfs` on a workstation is whatever
that machine happens to have (nothing at all on a Mac, 6.1 on Debian 12, 6.6 on
Ubuntu 24.04), and the kernel is worse, because a runner gets whichever one the cloud
booted that week. CI used to install xfsprogs on the runner, build the images there and
loop-mount them with `sudo`, while a developer got a Debian VM with a different
`mkfs.xfs` and a different kernel — so the two halves of the same gate were graded by
two different oracles. That is exactly what #211 and #212 are. There is one guest now,
supplied by
[fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness)
and configured by `fs-linux-test-harness.toml`; there is no host-side build and no
`sudo` anywhere.

```sh
chore siblings          # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore tools             # what the HOST needs: ripgrep and the VM. NOT xfsprogs
chore fixtures          # every set but stress, built in the guest, into .vm-share/
chore fixtures -- log truncate   # just those sets
chore fixtures -- stress         # asked for by name; see below
./scripts/test.sh --test oracle_vm_fixtures -- --nocapture
```

The sets build different things, and the difference is the reason each exists.
`geometry` formats a filesystem per entry in `scripts/fixture-geometries.sh` and
**never mounts it**, which is what the superblock and inode parsers need: nothing has
touched those images since `mkfs.xfs` wrote them. `log` mounts, writes and unmounts, so
the log keeps the records written along the way — a log with nothing in it cannot
disagree with us about how an item is written — and it varies the inode size, because a
logged inode addresses the *cluster* holding it by a rule that is not in the record.
`data` writes a tree somebody sat down and thought of — a small file, a big file, a
sparse file, several hundred directory entries — which is what the read path is walked
over. `truncate`, `create`, `unlink`, `dirconv`, `crossag`, `deeptree` and
`feature-matrix` are each the *before* state one suite of write tests needs the kernel
to have produced, so that what this driver does to them is measured against a volume it
did not make.

`stress` is the set whose contents nobody chose, and it is not in the default list: it
runs the two stress generators from the filesystem test suite against a mounted
filesystem and keeps what they leave behind, with a manifest of every path generated
inside Linux by the kernel's own driver. It is asked for by name because the generators
are built from source first and that takes tens of minutes — and a set quietly dropped
by a catch-all is the same shape of problem as an oracle that skips, so
`scripts/build-fixtures.sh` names `stress` as the one its default list leaves out
rather than letting "all" mean something narrower than it says.

**Licensing.** The filesystem test suite (fstests) is GPL-2.0. It is cloned, built and
executed **inside the guest only**; its source and its build artefacts never enter this
repository, nothing from it is copied, quoted or adapted here, and the two binaries are
invoked as external programs. Running a program does not make the caller a derivative
work of it; vendoring its code would. `scripts/guest-stress-tools.sh` records that, and
pins the build to a dated release tag so a rebuilt VM gets the same generators rather
than whatever upstream looks like that day.

**Nothing skips on a missing fixture.** The images are gitignored, and a test that
cannot find the one it needs fails naming `chore fixtures`. It used to print a skip line
and return `ok`, which reads exactly like a pass: `truncate_oracle` skipped on every CI
run from the day it was written, and `truncate.rs` sat at 5% line coverage underneath a
green suite. `scripts/ci-test.sh` is the other half of that guarantee — it fails a run
whose output matches a skip, and one that executed fewer tests than its floor.

`chore tools` installs what the **host** needs — ripgrep, and Vagrant, QEMU and KVM/HVF
for the VM — and says what each is for. It deliberately installs no xfsprogs: a
workstation that has none is a workstation on which no test can quietly ask the wrong
oracle, and `tests/test_contract.rs` fails the suite if one tries.

The harness's own tasks are available as `chore vm:up`, `vm:down`, `vm:status`,
`vm:run -- <cmd>`, `vm:exec -- <cmd>`, `vm:provision`, `vm:destroy` and
`vm:host:check`. `chore test:oracle` and `chore test:kernel` bring the VM up once for
the whole tier and leave it to the reaper, so thirty test binaries share one boot and
one multiplexed SSH connection.

## Building

```sh
cargo build --release
cargo clippy --all-targets -- -D warnings
```

Builds as both an `rlib` and a `staticlib`, so it links into a Rust dependency graph or
alongside sibling drivers in a C/Swift/Go consumer. Requires the sibling
`../rust-fs-core` checkout, which `chore siblings` clones and moves to the ref pinned
in `chores.yml`. `chore staticlib` builds the library and its headers into this crate's
own `dist/`, and `chore artifact` prints the absolute path of that directory — a
consumer copies its contents rather than being told where cargo puts things.

Install the git hooks once per clone:

```sh
~/.claude/skills/github-guard/install.sh .
```

The guards live in `.git/hooks`, outside the working tree, so no branch
checkout can rewrite the hook that is about to run. They are per-clone
rather than tracked: re-run the installer in a fresh clone, and after the
guards are updated.

## License

MIT — see [LICENSE](LICENSE).
