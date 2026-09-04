# Coverage, and what the number is worth

Measured 2026-09-05 with `cargo llvm-cov --all-features --summary-only`,
every fixture present.

**87.80% of lines, 90.27% of functions.**

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
built its fixtures.

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

Fixtures first, or the number will be lower and the suites will say so:

    sudo ./scripts/build-fixtures-native.sh
    sudo ./scripts/build-dirconv-fixtures.sh
    sudo ./scripts/build-truncate-fixtures.sh
    sudo ./scripts/build-unlink-fixtures.sh
    sudo ./scripts/build-create-fixtures.sh
    sudo ./scripts/build-log-fixtures.sh

Each needs xfsprogs and the privilege to loop-mount, so Linux — a CI
runner, a container, or the oracle VM via the matching `vm-build-*`
wrapper. The stress corpus is separate and slower; see
`.github/workflows/stress.yml`.
