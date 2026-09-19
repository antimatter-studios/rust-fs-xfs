#!/usr/bin/env bash
# Only HOST-side paths belong here.
#
# Scratch that a test or a fixture build writes on the host must live
# inside the checkout, because the oracle tools run inside the
# fs-linux-test-harness VM, which sees this repository and nothing else
# of the host: an image under /tmp or $RUNNER_TEMP is a path the tool
# asked to read it cannot open. scripts/with-test-temp.sh is the one
# place that chooses, and it now refuses a directory outside the
# repository rather than falling back to whatever the machine offered.
#
# GUEST-LOCAL PATHS ARE DELIBERATELY OUT OF SCOPE. Everything under
# scripts/guest-*.sh and scripts/build-*-fixtures.sh runs INSIDE the
# guest, where /var/tmp is the guest's own disk and is exactly where a
# fixture should be built — a loop mount of a file on the 9p share mixes
# two page caches over one file, and xfs_repair gets ENOTDIR from it.
# Those scripts are listed here so that the reason is written down, not
# so that they are searched.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# The host-side files that may name a writable path at all.
HOST_SIDE=(
    "$REPO/scripts/with-test-temp.sh"
    "$REPO/scripts/test.sh"
    "$REPO/scripts/ci-test.sh"
    "$REPO/scripts/build-fixtures.sh"
    "$REPO/scripts/tier.sh"
    "$REPO/.github/workflows/ci.yml"
)

command -v rg >/dev/null || {
    echo "FAIL  ripgrep is not installed; this check would report PASS having searched nothing" >&2
    echo "      Run 'chore tools'." >&2
    exit 1
}

matches="$({
    for f in "${HOST_SIDE[@]}"; do
        [[ -f "$f" ]] || { echo "$f: missing"; continue; }
        # `$REPO/tmp` is the scratch policy itself, so it is removed
        # before the search rather than excluded by a pattern that would
        # also hide a real /tmp beside it.
        # Comment lines are excluded: every one of these files explains
        # the policy at length, and an explanation that names /tmp is the
        # point rather than a violation of it.
        rg -n --no-heading '/tmp' "$f" \
            | rg -v '^[0-9]+:[[:space:]]*#' \
            | sed 's/\$REPO\/tmp//g; s/\$RUNNER_TEMP//g' \
            | rg '/tmp'
    done
} || true)"

if [[ -n "$matches" ]]; then
    echo "FAIL  writable host paths bypass the scratch policy:" >&2
    printf '%s\n' "$matches" >&2
    exit 1
fi

echo "PASS  writable host paths use the scratch policy"
