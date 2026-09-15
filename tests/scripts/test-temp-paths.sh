#!/usr/bin/env bash
# Only host-side paths belong here. Literal /tmp paths embedded in commands sent
# to the oracle VM are guest-local and deliberately outside this guard.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
matches="$({
    rg -n '/tmp' \
        "$REPO/scripts/build-fixtures-native.sh" \
        "$REPO/scripts/build-data-fixtures.sh" \
        "$REPO/.github/workflows/ci.yml"
    if [[ -f "$REPO/scripts/test.sh" ]]; then
        rg -n '/tmp' "$REPO/scripts/test.sh" "$REPO/scripts/with-test-temp.sh" \
            | sed 's/\$REPO\/tmp//g' \
            | rg -n '/tmp'
    fi
} || true)"

if [[ -n "$matches" ]]; then
    echo "FAIL  writable host fixture paths bypass the scratch policy:" >&2
    printf '%s\n' "$matches" >&2
    exit 1
fi

echo "PASS  writable host fixture paths use the scratch policy"
