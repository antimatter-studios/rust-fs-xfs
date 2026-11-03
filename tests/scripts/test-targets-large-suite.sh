#!/usr/bin/env bash
#
# test-targets-large-suite.sh — a suite's tier does not depend on its size.
#
# scripts/test-targets.sh decides a suite's tier by grepping its source,
# and it ran under `set -o pipefail` with `printf "$text" | grep -q`.
# `grep -q` exits at its first match; if printf has not finished writing,
# its next write takes SIGPIPE and the pipeline reports 141 -- which the
# classifier read as "no match". A suite that calls an oracle was then
# put in the UNIT tier, which runs with no VM, and failed there asking
# for one. It happened on main's test-darwin run for 9bbed0f, with
# tests/parent_exchrange_oracle.rs, and passed on the next run of the
# same commit: whether printf is still writing is a matter of timing.
#
# Timing cannot be a test, but size can: a source larger than a pipe's
# buffer guarantees printf is still writing when grep matches on the
# first line. So this builds a throwaway tree holding the real
# classifier and one large suite that calls an oracle, and requires the
# oracle tier -- and only the oracle tier -- to claim it.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
sandbox="$(mktemp -d)"
trap 'rm -rf "$sandbox"' EXIT
mkdir -p "$sandbox/scripts" "$sandbox/tests"
cp "$REPO/scripts/test-targets.sh" "$sandbox/scripts/"

# ~500 KB of code after one oracle call: eight times a 64 KB pipe.
{
    echo 'fn grade() { common::parent_oracle("mkfs.xfs"); }'
    for i in $(seq 1 12000); do
        echo "fn pad_$i() -> u32 { let value = $i; value + value }"
    done
} > "$sandbox/tests/large_oracle.rs"

# A CLASSIFIER THAT DIED CLAIMED NOTHING, which reads exactly like a tier
# that excluded the suite. Bash 3.2 refused the empty target array of
# every tier but oracle (#588), and this guard passed on the three errors,
# so each call has to succeed before what it printed counts as an answer.
#
# "$BASH", not `bash`: the classifier runs under the interpreter running
# this guard. `/bin/bash` here with a newer bash first on PATH would
# otherwise grade the newer one.
tiers_claiming() {
    local tier targets status failed=0
    for tier in unit images oracle kernel; do
        status=0
        targets="$("$BASH" "$sandbox/scripts/test-targets.sh" "$tier")" || status=$?
        if [ "$status" -ne 0 ]; then
            echo "FAIL  test-targets.sh $tier exited $status under bash $BASH_VERSION" >&2
            failed=1
            continue
        fi
        if grep -qx 'large_oracle' <<<"$targets"; then
            printf '%s ' "$tier"
        fi
    done
    return "$failed"
}

claimed="$(tiers_claiming)" || exit 1
if [ "$claimed" != "oracle " ]; then
    echo "FAIL  a large suite that calls an oracle is claimed by: '${claimed:-no tier}' (expected: oracle)" >&2
    exit 1
fi
echo "PASS  a large suite that calls an oracle is in the oracle tier only"
