#!/usr/bin/env bash
#
# ci-test.sh — run a test suite and treat a skip as a failure.
#
# WHY. Every fixture-gated test in this repository prints a skip line and
# returns ok when it cannot find its fixture. That is right for a fresh
# checkout and wrong in a job that just built the fixtures: the suite
# goes green having proved nothing. tests/truncate_oracle.rs skipped in
# CI for its whole existence that way, which is how truncate.rs came to
# sit at 5% line coverage with a passing oracle.
#
# WHY NOT A SEPARATE GATE STEP. The first version was one step at the end
# that re-ran everything and grepped. Running the suites twice made a
# test that is not idempotent fail the second time, and it doubled the
# job. Checking each suite as it runs is one execution and one place.
#
# Usage:  scripts/ci-test.sh --test truncate_oracle [more cargo args]
#         scripts/ci-test.sh --self-test
set -euo pipefail

# The exact wordings the suites use to say they skipped.
#
# PRECISE ON PURPOSE. This was `skipp|no fixture|unavailable`, which also
# matched "977 inode cores reproduced from disk, 0 skipped as stale" --
# a summary line from a suite that had just passed. A gate that fails on
# success is worse than no gate, so the patterns are anchored to how a
# skip is actually phrased and `--self-test` holds them to it.
SKIP_PATTERN='(^|[[:space:]])SKIPPED|no ([a-z0-9-]+ )?fixtures?|(VM|fixture) unavailable|skipped$|skipping( verification)?$'

if [ "${1:-}" = "--self-test" ]; then
    fail=0
    must_match=(
        "SKIPPED: no XFS fixture. Build one with sudo scripts/build-fixtures.sh"
        "spare: fixture or VM unavailable — skipped"
        "xfsstress-fsx: no fixture — skipping"
        "oracle VM unavailable — skipping verification"
        "no create fixtures or no VM; build them with ./scripts/vm-build-create-fixtures.sh"
        # Hyphenated and numbered names: the pattern had [a-z]+ and
        # missed both, which would have let a real skip through.
        "no feature-matrix fixtures — skipping. Build them with sudo ./scripts/build-feature-matrix-fixtures.sh"
        "no xfsfeat-reflink-finobt fixture — skipping"
        "no xfslog-b4096-i512 fixture — skipping"
    )
    must_not_match=(
        "977 inode cores reproduced from disk, 0 skipped as stale"
        "xfsstress-ops1k.img: 507 inodes matched, 1 records older than disk, 1 with a timestamp the disk moved on from"
        "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        "MOUNTED xfs-default — the in-kernel XFS driver accepted this image"
    )
    for line in "${must_match[@]}"; do
        echo "$line" | grep -qE "$SKIP_PATTERN" || { echo "MISSED a skip: $line" >&2; fail=1; }
    done
    for line in "${must_not_match[@]}"; do
        ! echo "$line" | grep -qE "$SKIP_PATTERN" || { echo "FALSE POSITIVE: $line" >&2; fail=1; }
    done
    [ "$fail" -eq 0 ] && echo "skip pattern behaves on all ${#must_match[@]} skip lines and ${#must_not_match[@]} others"
    exit "$fail"
fi

out=$(cargo test --locked --release "$@" -- --nocapture 2>&1) && status=0 || status=$?
echo "$out"

if [ "$status" -ne 0 ]; then
    exit "$status"
fi

if echo "$out" | grep -qE "$SKIP_PATTERN"; then
    echo "$out" | grep -E "$SKIP_PATTERN"
    echo "::error::a test skipped in a job that builds its fixtures — a skip is not a pass"
    exit 1
fi
