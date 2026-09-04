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
# WHY NOT A SEPARATE GATE STEP. The first version of this was one step at
# the end that re-ran everything and grepped. Running the suites twice
# made a test that is not idempotent fail the second time -- the run
# after a fixture has been written to is not the same run -- and it
# doubled the job. Checking each suite as it runs is one execution and
# one place.
#
# Usage:  scripts/ci-test.sh --test truncate_oracle [more cargo args]
set -euo pipefail

out=$(cargo test --locked --release "$@" -- --nocapture 2>&1) && status=0 || status=$?
echo "$out"

if [ "$status" -ne 0 ]; then
    exit "$status"
fi

# The wording every skip in this repository uses. Kept as one pattern so
# a new skip message has to be added here deliberately.
if echo "$out" | grep -qiE "skipp|no fixture|unavailable"; then
    echo "::error::a test skipped in a job that builds its fixtures — a skip is not a pass"
    exit 1
fi
