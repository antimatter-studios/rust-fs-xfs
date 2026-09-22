# Working in rust-fs-xfs (agent guide)

Pure-Rust XFS driver (`fs-xfs`) exposing a C ABI (`fs_xfs_*`). This file is the fast
path for an agent adding or fixing functionality, so the workflow doesn't have to be
re-derived each time. It points at the existing docs rather than duplicating them:

- **README** → `## Byte order`, `## Status`, `## Test contract`, `## Building`.

## The one rule that matters here

**Never validate this driver against fixtures the driver built itself.**

Unit tests that parse in-process fixtures prove self-consistency, not correctness. A
misreading of the on-disk format gets baked into the fixture *and* the parser, and they
agree with each other while disagreeing with every real filesystem. Three bugs shipped
past a fully green unit suite for exactly this reason:

| Bug | Why the unit tests missed it |
|---|---|
| Superblock magic had two bytes transposed (`0x58425346` for `0x58465342`) | Fixtures were written with the same wrong constant |
| Checksums read big-endian; XFS stores *checksums* little-endian (`~cpu_to_le32`) | Fixtures wrote them big-endian too |
| Checksum computed over the 264-byte struct, not the whole sector | Fixtures were 264 bytes |

All three died on the first comparison against `xfs_db`. Any new structure you parse
gets a cross-validation test in `tests/oracle_vm_fixtures.rs` before it is considered
done.

## Byte order

XFS is **big-endian on disk** — the only big-endian format in this crate family. Use
`from_be_bytes`, never `from_le_bytes`, never a raw struct cast. The sole exception is
checksum fields, which are little-endian; `superblock::le32` exists for those and
should not be used for anything else.

## Skills to use

- **`dev-loop`** — required for any non-trivial change: baseline the full suite →
  change → re-run (no baseline test may regress) → enhance tests → vet.
- **`commit`** / **`pr`** — for grouping commits and opening PRs. Commit subject plus
  flat one-sentence bullets; **no AI attribution or co-author trailers**.
- Discipline for **bug fixes**: **prove it's broken first** (a failing check), *then*
  fix, *then* prove the same check is green, *then* confirm the full baseline passes.
  Never write the fix before you have a red.

## Running tests

Every tier is a chore task, and every job in `.github/workflows/ci.yml` runs those same
tasks, so a green `chore test` here is the same evidence as a green run there.

```sh
chore siblings        # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore tools           # what the HOST needs: ripgrep, and the VM. NOT xfsprogs
chore fixtures        # the .vm-share images, built by the kernel in the guest
chore test:unit       # no tool, no fixture, no VM — debug profile, overflow checks on
chore test:images     # reads a fixture, needs no VM
chore test:oracle     # the driver writes, xfsprogs reads back
chore test:kernel     # the driver writes, the real kernel reads back
chore test:vm         # the whole suite compiled and run inside the guest
chore test:scripts    # the shell tests, tests/scripts/*.sh, by glob
chore test            # all of it, exactly as CI runs it
chore lint            # cargo fmt --check and clippy -D warnings, as CI runs them
```

A tier prints a verdict and writes everything it saw to `tmp/logs/<tier>.log`; add
`-- --verbose` to stream the run instead. Running one suite by hand still works:

```sh
./scripts/test.sh --test oracle_vm_fixtures -- --nocapture
```

**Nothing skips.** A missing tool, fixture or VM fails the test that needed it and
names the task that provides it, and `scripts/ci-test.sh` fails any run whose output
matches a skip or that executed fewer tests than its floor.

There is no hooks installer in this repository. `./scripts/install-hooks.sh` was
removed by #165, when the guards moved outside the working tree so that no branch
checkout could rewrite the hook about to run; this file went on naming it for far
longer. The hooks come from the github-guard skill's own installer, once per clone —
`~/.claude/skills/github-guard/install.sh .`, which writes them into `.git/hooks`,
as README `## Building` says.

## The oracle VM, and why everything Linux happens inside it

`mkfs.xfs`, `xfs_db`, `xfs_repair`, `xfs_logprint` and `xfs_io` — and the loop mounts
the kernel oracles need — run in one Debian guest, on every machine. Not "on a Mac,
where they are missing": always. xfsprogs on a workstation is whatever that machine
happens to have — nothing at all on a Mac, 6.1 on Debian 12, 6.6 on Ubuntu 24.04, 6.13
if somebody built one under `~/.local` — and an oracle whose answer depends on which
machine asked is not an oracle. CI used to install xfsprogs on the runner and loop-mount
there with `sudo` while a developer got a VM, so the branch gate and a local run were
graded by two different `mkfs.xfs` and two different kernels, which is what #211 and
#212 are. There is one guest now, and `tests/test_contract.rs` fails the suite if a
test reaches a tool any other way.

The VM belongs to
[fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness),
a sibling checkout at `../fs-linux-test-harness` that `chore siblings` moves to the tag
pinned in `chores.yml`. Thirteen VM scripts of this repository's own — `vm.sh`,
`vm-slot.sh`, `vm-session.sh` and ten `vm-build-*-fixtures.sh` wrappers — went with it;
its slot lock is the reconciliation of ours with two others. This repository configures
it in `fs-linux-test-harness.toml` and drives it through the tasks the harness supplies,
included here as `vm`:

```sh
chore vm:up            # boot, provision, and hold it up
chore vm:run -- <cmd>  # run a command as root in the guest, booting if needed
chore vm:exec -- <cmd> # the same in a VM already up — never boots one
chore vm:status        # exit 0 when it is running
chore vm:down          # halt, confirm it stopped (the next `up` is fast)
chore vm:destroy       # delete it and its disk; the next boot provisions from scratch
chore vm:provision     # re-run scripts/vm-setup.sh, changed or not
chore vm:host:check    # what this host is missing, with the command that installs it
```

`scripts/vm-setup.sh` is what the guest is provisioned with, and it is the only place
an oracle tool is installed: xfsprogs, `attr`/`acl`, a pinned xfsprogs 6.13 in its own
prefix (parent pointers arrived in 6.10 and Debian 12 ships 6.1, and everything else
keeps the distribution's `mkfs.xfs`, whose defaults the fixtures were built with), and
the Rust toolchain `chore test:vm` compiles the suite with.

Fixtures land in `.vm-share/` as `xfs-<name>.img` + `xfs-<name>.sbdump`, built by
`chore fixtures` — name sets to build only those, `chore fixtures -- log truncate`.
They are gitignored, and a missing one **fails** the test that wanted it naming that
task. It used to print a skip line and return `ok`, which is how `truncate.rs` came to
sit at 5% line coverage underneath a green oracle suite.

### Three VM traps, all paid for here

They belong to `../fs-linux-test-harness/vagrant/Vagrantfile` now — this repository's own
`tests/vagrant/debian/Vagrantfile` is gone — and
`../fs-linux-test-harness/tests/vagrantfile.sh` asserts every one of them, which is the
reason they moved rather than being re-learned by the next repository:

1. **Never set `config.notify_forwarder.enable = false`.** The plugin's `up` hook
   truncates the QEMU boot chain — the VM imports successfully and then never boots,
   printing no error at all. Either leave it enabled or don't load the plugin.
2. **`qe.virtiofs_guest_uid`/`gid` must match the box's `vagrant` user** (1001 on this
   box, not the plugin's 1000 default), or the shared folder is read-only to the guest
   and every fixture build fails with a bare permission error.
3. **`generic/debian12` is deliberately not used** — it was rebuilt upstream without its
   UEFI bootloader and no longer boots on a fresh clone. The box is `cloud-image/debian-12`.

## Adding a parsed structure

1. Derive field offsets from the format documentation. Do not trust recalled constants —
   two of the three bugs above were exactly that.
2. Where a `log2` companion field exists (`blocklog` beside `blocksize`, etc), assert
   they agree. That redundancy is the cheapest detector for a wrong offset or wrong
   byte order, and it is why XFS carries it.
3. On v5, verify both the CRC **and** the self-describing identity fields (UUID and
   owning AG). The checksum catches corrupted bits; the identity fields catch an
   intact block that came from the wrong place.
4. Add a cross-validation case to `tests/oracle_vm_fixtures.rs`, and — if the structure
   only appears under a particular mkfs option — a geometry to
   `scripts/fixture-geometries.sh`. That file is the single list: the builder
   (`scripts/guest-build-fixtures.sh`, in the guest) sources it, and
   `tests/scripts/fixture-geometries-single-copy.sh` fails if any builder grows a list
   of its own again. Two copies drifted once already (#110) — one list gained
   `nosparse` the other never built, and CI and a local run stopped meaning the same
   thing.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache). Shelling out to
  a copyleft CLI as a *test oracle* is fine — linking or copying is not.
- **This is a standalone project.** Never mention any consuming application in the
  README, source, or CLI help.
- Apple Silicon is the only target architecture for consumers of this crate; the crate
  itself is portable and CI runs on x86_64 Linux, which is fine because on-disk formats
  are endian-defined rather than host-defined.
