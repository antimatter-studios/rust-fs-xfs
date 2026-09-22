# Working in rust-fs-xfs (agent guide)

Pure-Rust XFS driver (`fs-xfs`) exposing a C ABI (`fs_xfs_*`), validated against
`xfs_db`, `xfs_repair`, `xfs_logprint`, `xfs_io` and the in-kernel XFS driver —
all of them inside one Debian guest. This file is the fast path for an agent
picking up work here, so the workflow does not have to be re-derived each time.
It points at the existing docs rather than duplicating them:

- **README** → `## Byte order`, `## Status`, `## Test contract`, `## Building`.
- **`chores.yml`** → every task named below, and what each one actually runs.
- **`.github-guard`** → what must pass before `main` takes a merge.

The section between the BEGIN/END markers below is **shared, byte-identical,
with every repository in this family**. Do not edit it here: change the
canonical copy and propagate it, or `scripts/agents-core-check.sh` will fail.
Everything after the END marker is specific to this repository.

<!-- BEGIN SHARED BLOCK: agent-core v1 sha256:60fad6dd98e9da3e9256d38728b02ac189dca0d04fc98c13e2c67de3f3103319 -->
## Claiming work

Several agents work these repositories at the same time. Before you start on
an issue, claim it, so nobody else spends a session on what you are already
doing. The lock is a **GitHub label**, because labels are shared state that
every agent can read and change without posting comments into the thread.

**Before starting.** Check, claim, then read back:

```sh
gh issue view <N> --json labels                      # holds `claimed`? pick another
gh issue edit <N> --add-label claimed --add-label claim/<session>
gh issue view <N> --json labels                      # read back and confirm
```

`<session>` is your session name — `agent-<random4>-<isodate>`, e.g.
`agent-3f7c-2026-09-22`. Create the `claim/<session>` label if it does not
exist.

**Resolving a race.** Adding a label is not compare-and-swap: two agents can
both add `claimed` and both believe they won. That is what the read-back is
for. If it shows more than one `claim/*` label, the **lexically lowest**
session keeps the issue; every other agent removes its own `claim/*` label and
picks different work. Each racer computes the same answer independently, so no
further coordination is needed.

**When you finish or stop.** Remove both labels — on merge, or the moment you
abandon the work:

```sh
gh issue edit <N> --remove-label claimed --remove-label claim/<session>
```

Delete your `claim/<session>` label from the repository at the end of your
session so they do not accumulate.

**Reclaiming a stale claim.** An agent that dies holding a claim would block an
issue forever. If `claimed` was applied more than 12 hours ago and the holder's
branch has no commits since, any agent may take it: remove the stale `claim/*`,
add your own, and say so in the issue.

**This is a convention, not a fence.** Nothing enforces it. An agent that
ignores it duplicates work; it cannot corrupt anything. Honour it anyway.

## Skills to use

- **`dev-loop`** — the required loop for any non-trivial change: baseline the
  full suite → change → re-run (no baseline test may regress) → enhance tests →
  vet. Always run it.
- **`commit`** / **`pr`** — for grouping commits and opening pull requests.

Each repository names any further skills of its own below.

## A bug fix starts with a red

**Prove it is broken first** — a failing check or test — *then* fix it, *then*
prove that same check is green, *then* confirm the full baseline still passes.
Never write the fix before you have a red. A fix with no failing test to its
name is a claim, not a result.

## Nothing skips

A test that cannot run **fails**, naming the task that would provide what it
needed. Never add an early return for a missing fixture, tool or VM: a skipped
test reads exactly like a passing one, and a suite that quietly declines to run
is indistinguishable from a suite that passes.

Where a tier reports skips or ignored tests, that is a gate, not a note.

## Validate against something that is not us

A driver's own readers share its interpretation of the format, so they cannot
catch a misreading: the mistake is baked into the fixture *and* the parser, and
they agree with each other while disagreeing with every real filesystem. Unit
tests over self-built fixtures prove self-consistency, not correctness.

Every structure that is parsed or written gets a cross-validation test against
an **independent oracle** — the platform's own tools, a real kernel, or a third
implementation — before it is considered done. Each repository names its
oracles below.

## Output is budgeted

Test tiers run through `scripts/tier.sh`, which runs the suite **quietly**: the
whole run goes to `tmp/logs/<tier>.log`, a pass prints one verdict line naming
that log, and a failure prints its tail. CI keeps the logs as an artifact, so
the detail is always retrievable.

The budget caps the log, not merely what is shown, and every number in the
table was measured. A run that passes but prints more than its budget **fails**.

The reader who pays most for a noisy suite is an agent that re-reads its whole
transcript on every step, and so pays for one loud run many times over. If a
tier legitimately grows, raise its row **with the measurement that justifies
it**. Do not silence output to fit, and do not route around `tier.sh`.

## Commits and branches

- Branches are `<type>/<name>`, matching the commit type: `fix/`, `feat/`,
  `ci/`, `docs/`, `chore/`, `test/`.
- A commit is a subject plus flat one-sentence bullets. Subjects are
  declarative, not imperative: "the run-end bound is checked", not "check the
  run-end bound".
- **No AI attribution and no co-author trailers**, in commits or in pull
  request descriptions.
- `main` takes **squash merges only**.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache).
  Shelling out to a copyleft CLI as a *test oracle* is fine — linking or
  copying it is not.
- **Each of these is a standalone project.** Never mention a consuming
  application in the README, the source, or CLI help.
<!-- END SHARED BLOCK: agent-core v1 -->

## What "validate against something that is not us" cost here

The shared block states the rule. This is the bill. Three bugs shipped past a
fully green unit suite, because the fixtures and the parser shared one
misreading and agreed with each other:

| Bug | Why the unit tests missed it |
|---|---|
| Superblock magic had two bytes transposed (`0x58425346` for `0x58465342`) | Fixtures were written with the same wrong constant |
| Checksums read big-endian; XFS stores *checksums* little-endian (`~cpu_to_le32`) | Fixtures wrote them big-endian too |
| Checksum computed over the 264-byte struct, not the whole sector | Fixtures were 264 bytes |

All three died on the first comparison against `xfs_db`. Any new structure you
parse gets a cross-validation test in `tests/oracle_vm_fixtures.rs` before it is
considered done.


## Byte order

XFS is **big-endian on disk** — the only big-endian format in this crate family. Use
`from_be_bytes`, never `from_le_bytes`, never a raw struct cast. The sole exception is
checksum fields, which are little-endian; `superblock::le32` exists for those and
should not be used for anything else.


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

## What gates a merge

**One required check: `ci-ok`.** It carries `if: always()`, `needs:` every other
job in `ci.yml` — `unit`, `fixtures`, `test`, the aarch64 tiers and the in-guest
suite — and fails when any of them failed, was cancelled or was **skipped**. It
runs no tests of its own: it is a claim about the other jobs, so it must not
pass work of its own off as theirs.

`.github-guard` requires that one name and nothing else, and github-guard reads
it from the **server copy of the default branch**, never the working tree —
which is what stops a branch checkout from unprotecting `main`.

`chore check:ci-gate` holds both halves of that mechanically — every job in
`ci.yml` must appear in `ci-ok`'s `needs:`, and `.github-guard` must require
`ci-ok` and nothing else. The task names `scripts/ci-gate.sh` and nothing else,
so the script is what can be tested, reviewed and run without `chore` at all.
It replaced `tests/ci_aggregate_gate.rs`: that parsed a YAML file and compared
strings, exercising nothing this crate ships, and as a `cargo test` it counted
towards the executed-test floor the gate itself enforces.

`release.yml` triggers on a tag and `fuzz.yml` is dispatch plus a nightly cron,
so neither ever reports on a pull request and neither may gate. A required
context no job produces reads to GitHub as permanently *pending*, not failing.

Judging mergeability from check **conclusions** is unreliable: an in-progress
`CheckRun` reports its conclusion as an empty string, and a `StatusContext` has
no conclusion field at all. Read `mergeStateStatus` and
`statusCheckRollup.state`.

## Project rules specific to this crate

- Apple Silicon is the only target architecture for consumers of this crate; the
  crate itself is portable and CI runs on x86_64 Linux, which is fine because
  on-disk formats are endian-defined rather than host-defined.

## Never grow a shared tool to solve a problem in this repository

`chore` is a general-purpose task runner this project merely consumes; the same
goes for `github-guard` and the agent-skills hooks. If something needed here
looks like it belongs inside one of them, it does not. Solve it here, or ask
first. The tell is a release: if a shared tool needs a new version cut whose
only purpose is to unblock this project, the code is in the wrong repository.

## The shared block above is checked

`scripts/agents-core-check.sh` hashes the content between the BEGIN/END markers
and compares it against the canonical digest. Editing the block here fails that
check rather than quietly giving this repository its own rules. When the block
genuinely changes it changes everywhere: edit the canonical copy, then update
the digest in the script and in the BEGIN marker of every repository.
