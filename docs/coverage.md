# Coverage, and what the number is worth

Measured 2026-09-05 with `cargo llvm-cov --all-features --summary-only`,
every fixture present.

**88.49% of lines, 91.06% of functions.**

The write path has moved a long way since the first figure below: the
reverse-mapping and reference-count trees are maintained rather than
refused, and `tests/feature_matrix_oracle.rs` now runs every write
operation against every legal combination of the features and encodings
that change what a write has to do. See `docs/write-support.md`.

    create.rs     94.53%      rmap.rs       97.03%
    dir_block.rs  97.71%      refcount.rs   97.95%
    dir_write.rs  91.08%      unlink.rs     86.43%
    fs.rs         82.92%      file_write.rs 78.55%
                              truncate.rs   77.27%

## How it moved, and why that is the interesting part

It read 80.03% before this work and none of the gain came from writing
tests. It came from making tests that already existed actually run.

| module | before | after | what changed |
|---|---|---|---|
| `truncate.rs` | 5.41% | 70.27% | its fixtures had only a VM builder |
| `unlink.rs` | 25.62% | 84.30% | same |
| `dir_write.rs` | 38.43% | 89.55% | `rename_oracle` passed in 0.00s with no fixture |
| `file_write.rs` | 27.71% | 77.91% | same fixtures |
| `create.rs` | 80.89% | 92.24% | same |
| `group_write.rs` | 76.23% | 87.70% | same |
| `fs.rs` | 73.67% | 85.05% | same |

Every one of those suites reported passes the whole time. A fixture-gated
test prints a skip line and returns `ok`, so a suite that cannot find its
images looks exactly like one that ran. `truncate_oracle` had skipped on
every CI run since it was written; all five tests in `rename_oracle`
passed in **0.00 seconds** for want of one image.

That is the thing to distrust in this repository, and `scripts/ci-test.sh`
exists to make it loud: it fails a job when a suite skips in a job that
built its fixtures, and — since #201 — when a run executes fewer tests
than the floor it was given. A skip is the loud way for a suite to prove
nothing. Executing nothing at all is the quiet one: the harness exits 0
on `0 passed; 0 failed` and prints no skip line for the gate to find, so
only a count of what ran can tell that apart from a full run. Every test
run carries a floor measured from a real CI log, with the count, the date
and the run id beside it.

**Every tier goes through that script now**, which is what makes the
guarantee worth anything. It used to be reachable only from the suites
`ci.yml` happened to name in a `for suite in ...` list — so a suite left
out of the list was a suite whose skip nobody checked, which is the same
failure the list was written to prevent. The tiers are
`chore test:unit`, `test:images`, `test:oracle`, `test:kernel`,
`test:vm` and `test:scripts`, and each of them either runs `ci-test.sh`
or hands it the log to gate (`--gate`, which the debug unit tier uses
because `ci-test.sh`'s run mode pins `--release`).

The gate's own `--self-test` — which holds the skip pattern to the exact
wordings the suites use, in both directions, so that neither a real skip
slips through nor a passing summary line is failed for saying "skipped" —
**had never run once**, from the day it was written until #200. It is a
shell test now: `tests/scripts/ci-test-self-test.sh`, picked up by
`chore test:scripts` by glob, because a guard that has to be registered
somewhere before it runs is a guard that gets silently skipped.

## What the corpus caught once it ran

- **`bmbt` blocks in a later allocation group were read at the wrong
  address.** A packed fsbno is not a linear block number, and the two
  only agree when `agblocks` is a power of two — which the unit-test
  superblock was, and real filesystems are not.
- **A log record can sit behind the disk by a timestamp alone.**
  `di_changecount` is not bumped for a timestamp-only update, so an equal
  count does not mean an equal inode.

Both were invisible to hand-built fixtures. Real files land in later
groups; hand-written ones do not.

## What is still low, and which of it matters

| module | lines | worth chasing? |
|---|---|---|
| `format/dir.rs` | 3.41% | **No.** Metric artifact — see below. |
| `format/attr.rs` | 38.33% | Partly, same reason. |
| `truncate.rs` | 70.27% | Yes: 3 of 6 functions unexecuted. |
| `inode_btree.rs` | 74.06% | Yes. |
| `file_write.rs` | 77.91% | Yes: 4 of 12 functions unexecuted. |

### Why `format/` is not a target

`format/dir.rs` is 1043 lines of which almost all are `pub const`
declarations — offsets, magic numbers, feature bits. A constant is never
"executed", so it can never be covered, and the percentage measures how
much of the file is documentation of the on-disk layout rather than how
much of it is checked.

The real logic there is about 35 `const fn`s — `buf_space`,
`leaf_first_fsb`, `hashname`, `rmt_blocks`. Those are worth testing, and
they are worth testing **against values read off real images**, not
against themselves. A test asserting that a constant equals its own
literal proves the constant was typed twice.

The constants are already checked, and more strongly than a unit test
would: `tests/oracle_mkfs.rs` compares every field this driver parses
against what `xfs_db` reports for the same field, on filesystems
`mkfs.xfs` built. A wrong offset fails there against the reference
implementation, which is the only opinion that counts.

## Reproducing

    cargo llvm-cov --all-features --summary-only

Fixtures first, or the number will be lower and the suites will fail
saying so:

    chore siblings
    chore fixtures

One task for every set, because there is one place the fixtures are
built. Each of these images needs the canonical `mkfs.xfs` and, for the
populated ones,
the real kernel's XFS driver and the privilege to loop-mount — so all of
it happens inside the fs-linux-test-harness guest, on every machine.
There is no host-side builder and no `sudo` in this repository any more.
The pair that used to be here — a native builder for a CI runner and a
VM builder for a developer's loop — were the same work written twice,
they had already drifted once (#110), and while they both existed a
fixture could be built by one kernel and graded by another, which is
what #211 and #212 are.

`chore fixtures -- dirconv truncate unlink create log` rebuilds just the
sets a particular row above depends on. The stress corpus is not in the
default set because its generators are built from source first: ask for
it by name with `chore fixtures -- stress`, or let the weekly
`.github/workflows/stress.yml` run do it, which builds it in the same
guest.
