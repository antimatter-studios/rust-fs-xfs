#!/usr/bin/env bash
#
# ci-test-self-test.sh — run `scripts/ci-test.sh --self-test`.
#
# THE GATE'S OWN GATE, AND IT NEVER RAN (#200). ci-test.sh is what stops
# a skip reading as a pass in this repository, and it carries a self-test
# holding its skip pattern to eleven real skip lines, its five
# near-misses (progress lines that say "skipped" about data, which an
# earlier looser pattern failed a passing suite on), the executed-test
# counter against six shapes of cargo output, and the floor itself
# against a run that executed nothing. All of it was dead code: no
# workflow, no task and no test invoked `--self-test`, so the patterns
# that decide whether this repository's gate works were never checked by
# anything.
#
# It lives here, among the shell tests, because that is the tier that
# runs every tests/scripts/*.sh by glob — a guard that has to be
# registered somewhere else is a guard that gets silently skipped, which
# is the mistake this file exists to stop repeating.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

if ! output="$("$REPO/scripts/ci-test.sh" --self-test 2>&1)"; then
    echo "FAIL  scripts/ci-test.sh --self-test:" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
printf '%s\n' "$output"
echo "PASS  the skip gate and the executed-test floor behave as written"
