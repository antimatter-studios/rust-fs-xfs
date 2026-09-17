#!/usr/bin/env bash
#
# ci-test.sh — run a test suite, treat a skip as a failure, and refuse a
# run that executed nothing.
#
# WHY. Every fixture-gated test in this repository prints a skip line and
# returns ok when it cannot find its fixture. That is right for a fresh
# checkout and wrong in a job that just built the fixtures: the suite
# goes green having proved nothing. tests/truncate_oracle.rs skipped in
# CI for its whole existence that way, which is how truncate.rs came to
# sit at 5% line coverage with a passing oracle.
#
# WHY A COUNT AS WELL AS THE SKIP PATTERN (#201). A skip is the loud way
# for a suite to prove nothing; executing no tests at all is the quiet
# one, and the gate below cannot see it. The harness exits 0 on
# "0 passed; 0 failed", so a `--test` argument naming a suite that no
# longer exists under that name, a filter that selected nothing, a build
# that produced no test binaries, and a suite whose every case was
# deleted all report the same green as a full run — and none of them
# prints a skip line, because nothing ran to print one. Only a count sees
# that, so every run through this script must execute at least
# CI_TEST_FLOOR tests (default 1) or the job fails.
#
# WHY NOT A SEPARATE GATE STEP. The first version was one step at the end
# that re-ran everything and grepped. Running the suites twice made a
# test that is not idempotent fail the second time, and it doubled the
# job. Checking each suite as it runs is one execution and one place.
#
# Usage:  scripts/ci-test.sh --test truncate_oracle [more cargo args]
#         CI_TEST_FLOOR=5 scripts/ci-test.sh --test oracle_vm_fixtures
#         scripts/ci-test.sh --floor-check <floor> <logfile> <label>
#         scripts/ci-test.sh --self-test
#
# THE FLOOR IS PASSED IN THE ENVIRONMENT, NOT AS AN ARGUMENT, and that is
# not a style choice. tests/every_fixture_suite_is_run_gated.rs reads the
# workflows for the literal shape `ci-test.sh --test <suite>` to decide
# which fixture-gated suites are run where a skip fails; an option
# between the script and `--test` makes every suite in that step read as
# unrun, and that guard goes red for a reason with nothing to do with
# fixtures. An environment prefix leaves the shape intact.
set -euo pipefail

# The exact wordings the suites use to say they skipped.
#
# PRECISE ON PURPOSE. This was `skipp|no fixture|unavailable`, which also
# matched "977 inode cores reproduced from disk, 0 skipped as stale" --
# a summary line from a suite that had just passed. A gate that fails on
# success is worse than no gate, so the patterns are anchored to how a
# skip is actually phrased and `--self-test` holds them to it.
#
# STILL ANCHORED, AND NAMING WHAT IS SKIPPED RATHER THAN MATCHING ANY
# TAIL. `skipping the check` and `skipping that comparison` are listed
# words, not `skipping.*`, for the reason above: the next progress line
# to mention skipping must not fail a passing suite.
SKIP_PATTERN='(^|[[:space:]])SKIPPED|no ([A-Za-z0-9-]+ )?fixtures?|(VM|fixture) unavailable|skipped$|skipping( verification| the check| that comparison)?$'

# How many tests actually ran, summed from the harness's own summary
# lines, reading the output on stdin.
#
# `test result: ok. N passed` is printed once per test binary, so a run
# covering several binaries sums. Only the `ok.` form counts: a failing
# binary prints `test result: FAILED. N passed; M failed`, and a run that
# failed is already a failure -- crediting its passes towards the floor
# would let a half-executed suite buy its way over the line.
#
# awk, not bc or a shell accumulator, because this also runs on the macOS
# runner where bc is not installed. `grep || true` because grep exits 1
# when it matches nothing, which under `set -o pipefail` would abort the
# script at exactly the moment the answer (zero) matters most.
executed_tests() {
    { grep -aoE 'test result: ok\. [0-9]+ passed' || true; } | awk '{ s += $4 } END { print s + 0 }'
}

# Refuse a run that executed fewer than `floor` tests, reading the output
# on stdin. `label` names the run selection in the error, because the
# floors are per selection -- a `--ignored` run and the default run
# select disjoint sets, and either can empty without the other moving --
# and a reader needs to know which one emptied.
enforce_floor() {
    local floor=$1 label=$2 count
    count=$(executed_tests)
    echo "tests executed: $count (floor $floor) — $label"
    if [ "$count" -lt "$floor" ]; then
        echo "::error::only $count tests executed by $label, floor is $floor — a run below its floor stopped early, selected nothing, or was built from nothing, and none of those is a pass"
        return 1
    fi
}

# The counting half on its own, for the workflow steps that do not go
# through this script's run mode: the plain suite runs in the test and
# darwin jobs, and the `--ignored` run whose bypass is #200. They tee
# their output to a log and hand it here, so the arithmetic, the wording
# and the self-test below are shared rather than copied four times.
if [ "${1:-}" = "--floor-check" ]; then
    enforce_floor "$2" "${4:-$3}" < "$3"
    exit $?
fi

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
        # The crate's own name in its own casing, and a skip that says
        # what it skipped after the word (#164). Each returned from a test
        # body with nothing asserted, and the pattern saw none of them:
        # the class was lowercase-only and the `skipping` clause ended at
        # the word.
        "no XFS fixtures found; build them with ./scripts/vm-build-log-fixtures.sh"
        "no kernel to replay the record — skipping the check"
        "note: xfs_db did not report \`sb_icount\`, skipping that comparison"
    )
    must_not_match=(
        "977 inode cores reproduced from disk, 0 skipped as stale"
        "xfsstress-ops1k.img: 507 inodes matched, 1 records older than disk, 1 with a timestamp the disk moved on from"
        "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
        "MOUNTED xfs-default — the in-kernel XFS driver accepted this image"
        # A progress line of the stale-cores shape, which a looser
        # `skipp` clause would take for a skip.
        "re-encoded 212 kernel buffer items (3 skipped as split across records)"
    )
    for line in "${must_match[@]}"; do
        echo "$line" | grep -qE "$SKIP_PATTERN" || { echo "MISSED a skip: $line" >&2; fail=1; }
    done
    for line in "${must_not_match[@]}"; do
        ! echo "$line" | grep -qE "$SKIP_PATTERN" || { echo "FALSE POSITIVE: $line" >&2; fail=1; }
    done
    # The counter, held to the outputs it has to read. Each case is a
    # shape CI has produced or can produce: the ordinary summary, several
    # binaries in one run, the empty green the floor exists for, a filter
    # that selected nothing, and a failing binary whose passes must not
    # count towards anyone's floor.
    counted=0
    count_is() {
        local want=$1 got
        counted=$((counted + 1))
        got=$(printf '%s\n' "$2" | executed_tests)
        [ "$got" = "$want" ] || { echo "COUNTED $got, wanted $want, in: $2" >&2; fail=1; }
    }
    count_is 3 "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.13s"
    count_is 21 "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
running 18 tests
test result: ok. 18 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out"
    count_is 0 "running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"
    count_is 0 "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 41 filtered out; finished in 0.00s"
    count_is 0 "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out"
    count_is 0 ""

    # The gate itself, not just its arithmetic: a run that executed
    # nothing must fail, and an ordinary one must not.
    if printf 'test result: ok. 0 passed; 0 failed\n' | enforce_floor 1 self-test > /dev/null 2>&1; then
        echo "THE FLOOR PASSED A RUN THAT EXECUTED NOTHING" >&2
        fail=1
    fi
    if ! printf 'test result: ok. 3 passed; 0 failed\n' | enforce_floor 3 self-test > /dev/null 2>&1; then
        echo "THE FLOOR FAILED A RUN THAT MET IT" >&2
        fail=1
    fi

    [ "$fail" -eq 0 ] && echo "skip pattern behaves on all ${#must_match[@]} skip lines and ${#must_not_match[@]} others, and the executed-test count on $counted outputs"
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

# The floor for this run. One is the only number that needs no
# maintenance and it is already the whole point: every suite this
# repository hands to the script executes at least one test, and a
# selection that executed none proved nothing whatever it reported. A
# caller that knows how many its selection should execute raises it with
# CI_TEST_FLOOR — see ci.yml, where the two single-suite oracle steps
# carry the count their suite produced on 2026-09-17.
echo "$out" | enforce_floor "${CI_TEST_FLOOR:-1}" "${*:-the default selection}"
