# How this crate is tested

The test contract, the tiers, and how the fixtures the tests read are made.
`AGENTS.md` (`## Running tests` and `## The oracle VM, and why everything Linux happens
inside it`) is the short version for someone about to run them; this is the reasoning
behind it.

## Two layers

Two layers, and the second is the one that matters.

**1. Unit tests** (`src/`) parse structures built in-process. These prove the parser is
*self-consistent*. They cannot prove it is *correct*, because a misreading of the
format is baked into both the fixture and the parser, and they agree with each other
while disagreeing with the rest of the world.

**2. Cross-validation against real XFS tooling and the Linux kernel** (`tests/`).
Filesystems are built by the canonical `mkfs.xfs` and dumped with `xfs_db`; this driver
parses the same images and every field it reports must match the value `xfs_db` reports
for that field (`tests/oracle_vm_fixtures.rs`). What the driver writes is read back by
`xfs_repair -n`, `xfs_db` and `xfs_logprint`, and mounted by the Linux kernel, which
replays every log record the driver wrote. The geometries are listed once, in
`scripts/fixture-geometries.sh`: 1k/2k/4k blocks, 512b/1k inodes, explicit
allocation-group counts, `reflink+rmapbt`, `bigtime`, a v4 filesystem with no CRC at
all, and a v5 one with sparse inodes off.

The second layer is not optional decoration: three bugs shipped past a fully green unit
suite and died on the first comparison against `xfs_db`
([on-disk-format.md](on-disk-format.md#what-only-an-oracle-caught) lists them).

## Running the tiers

```sh
chore test:unit    # no tool, no fixture, no VM; green after `chore siblings`
chore test:images  # adds the tests that read a fixture, still without a VM
chore test         # everything, exactly as CI runs it
./scripts/test.sh --test oracle_vm_fixtures -- --nocapture   # one suite, by hand
```

Every tier is a task in `chores.yml`, and CI runs those same tasks; `AGENTS.md` lists
them all (`test:oracle`, `test:kernel`, `test:kernel-new`, `test:stress`, `test:vm`,
`test:scripts`, `test:cli`, `lint`). A tier runs quietly and writes everything it saw
to `tmp/logs/<tier>.log`.

There is no `--ignored` tier and no host-side oracle. Every test that runs a tool runs
it in the guest, and a test that cannot reach one **fails** rather than reporting `ok`
— see [Nothing skips](#nothing-skips) below.

## Scratch directories

`scripts/test.sh` and every tier go through `scripts/with-test-temp.sh`, which exports
an isolated `TMPDIR` and cleans the child it owns. Scratch always lives inside the
repository, under `tmp/` (gitignored), never in the system temporary directory or a
runner-supplied one. That is not only tidiness: the guest sees this repository and
nothing else of the host, so an image under `/tmp` or `$RUNNER_TEMP` is a path the tool
asked to read it cannot open. An exact `FS_XFS_TEST_TMPDIR` is caller-managed;
`FS_XFS_TEST_TMP_BASE` receives a unique child; both must be inside the repository,
and the wrapper refuses either outside it.

The wrapper also runs the command inside a harness session, so the run owns the VM its
tests use: the VM comes down and the machine-wide slot is released when the run ends,
passed, failed or killed, unless it was held up with `chore vm:up`.

## The oracle VM

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
`sudo` anywhere. A second guest, configured by `fs-linux-test-harness.debian-13.toml`
(kernel 6.12), runs `chore test:kernel-new` for what the first one's kernel cannot
mount, such as parent pointers and exchange-range.

```sh
chore siblings          # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore tools             # what the HOST needs: ripgrep and the VM. NOT xfsprogs
chore fixtures          # every set but stress, built in the guest, into .vm-share/
chore fixtures -- log truncate   # just those sets
chore fixtures -- stress         # asked for by name; see below
./scripts/test.sh --test oracle_vm_fixtures -- --nocapture
```

`chore tools` installs what the **host** needs — ripgrep, and Vagrant, QEMU and KVM/HVF
for the VM — and says what each is for. It deliberately installs no xfsprogs: a
workstation that has none is a workstation on which no test can quietly ask the wrong
oracle, and `tests/test_contract.rs` fails the suite if one tries.

The harness's own tasks are available as `chore vm:up`, `vm:down`, `vm:status`,
`vm:run -- <cmd>`, `vm:exec -- <cmd>`, `vm:provision`, `vm:destroy` and
`vm:host:check`. `chore test:oracle` and `chore test:kernel` bring the VM up once
before the tier starts, so every test binary in the tier shares one boot and one
multiplexed SSH connection.

## Fixture sets

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
did not make. `cli` is what the command-line tools are tested on: a v5 and a v4 image,
each made by `mkfs.xfs -L` and filled by the kernel with a tree meant to reach every way
XFS stores a file, a manifest of that tree written by the kernel's own driver, and the
offsets `xfs_db` gives for the bytes the tier breaks in a copy.

`stress` is the set whose contents nobody chose, and it is not in the default list: it
runs the two stress generators from the filesystem test suite against a mounted
filesystem and keeps what they leave behind, with a manifest of every path generated
inside Linux by the kernel's own driver. It is asked for by name because the generators
are built from source first and that takes tens of minutes — and a set quietly dropped
by a catch-all is the same shape of problem as an oracle that skips, so
`scripts/build-fixtures.sh` names `stress` as the one its default list leaves out
rather than letting "all" mean something narrower than it says. `chore test:stress`
reads it back.

**Licensing.** The filesystem test suite (fstests) is GPL-2.0. It is cloned, built and
executed **inside the guest only**; its source and its build artefacts never enter this
repository, nothing from it is copied, quoted or adapted here, and the two binaries are
invoked as external programs. Running a program does not make the caller a derivative
work of it; vendoring its code would. `scripts/guest-stress-tools.sh` records that, and
pins the build to a dated release tag so a rebuilt VM gets the same generators rather
than whatever upstream looks like that day.

To add a geometry or a parsed structure, see `## Adding a parsed structure` in
`AGENTS.md`.

## Nothing skips

The images are gitignored, and a test that cannot find the one it needs fails naming
`chore fixtures`. It used to print a skip line and return `ok`, which reads exactly like
a pass: `truncate_oracle` skipped on every CI run from the day it was written, and
`truncate.rs` sat at 5% line coverage underneath a green suite. `scripts/ci-test.sh` is
the other half of that guarantee — it fails a run whose output matches a skip, and one
that executed fewer tests than its floor.

## The command-line tools

The tools are tested as installed, found on `PATH`:

```sh
chore cli:install                          # tmp/cli/bin, and the PATH line to use
export PATH="$(scripts/cli-install.sh --print-bin-dir):$PATH"
chore test:cli                             # doctor first, then tests/cli/*.sh
```

What the tools' output is held to, beyond that tier:

- `tests/cli_fsck_oracle.rs` holds `fsck.xfs` to `xfs_repair -n` on twelve clean
  fixtures and on damage for every finding code the reference can grade, including
  golden JSON checks for codes, severity, locations and incomplete scans. `tests/fsck_coverage_contract.rs`
  keeps the code catalogue in [fsck-output.md](fsck-output.md) and those cases in step.
- `tests/cli_repair_plan_oracle.rs` holds the repair plan to `xfs_repair -n`, requires
  it to be the same twice, and requires the image unchanged.
- `tests/cli_superblock_repair_oracle.rs` repairs a copy, then has `xfs_repair -n` and
  `xfs_db` confirm the repair, and shows that a second run writes nothing. The other
  repair rules have their own kernel and `xfs_repair` suites, named on the
  [features page](features.md#checking).
- `tests/cli_mkfs_kernel.rs` has `xfs_repair -n` and the kernel accept what `mkfs.xfs`
  makes, and `tests/mkfs_layout.rs` requires every allocation group header and btree
  root to be byte-identical to the standard formatter's on the same device with the
  same UUID.

## Fuzzing

The parsers are fuzzed on two tiers. `fuzz/` holds `cargo-fuzz` targets (the superblock,
the group headers and free list, inodes, extent lists, directories, btrees and the
logged inode core), run by
`scripts/fuzz-all.sh` for a bounded time on nightly and by a scheduled workflow; it is
the explorer, and it is not a required check. `tests/fuzz_decoders.rs` is the gate: it
replays `fuzz/corpus/` and mutates it on the stable toolchain in every pull request,
so anything the explorer finds is checked from then on.
