#!/usr/bin/env bash
#
# fixture-geometries-single-copy.sh — both fixture builders read the one
# geometry list (#110).
#
# The native (CI) and VM (developer) builders each carried a GEOMETRIES
# array, and they drifted: CI never built `nosparse`, and the VM left
# `default` unpinned. Now both source scripts/fixture-geometries.sh. This
# fails if either grows its own list again or stops sourcing the shared
# one, and pins the two entries whose drift was the defect.
#
#   bash tests/scripts/fixture-geometries-single-copy.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
check() {
    if eval "$1"; then
        printf 'ok    %s\n' "$2"
    else
        printf 'FAIL  %s\n' "$2"
        fails=$((fails + 1))
    fi
}

for builder in build-fixtures-native.sh vm-build-fixtures.sh; do
    f="$REPO/scripts/$builder"
    check "grep -q 'source \"\$REPO/scripts/fixture-geometries.sh\"' '$f'" \
        "$builder sources the shared geometry list"
    check "! grep -qE '^[[:space:]]*[A-Z_]*GEOMETRIES=\\(' '$f'" \
        "$builder declares no geometry list of its own"
    check "grep -q '\"\${XFS_GEOMETRIES\[@\]}\"' '$f'" \
        "$builder builds from that list"
done

# shellcheck source=/dev/null
source "$REPO/scripts/fixture-geometries.sh"
has() {
    local g
    for g in "${XFS_GEOMETRIES[@]}"; do [ "$g" = "$1" ] && return 0; done
    return 1
}
check 'has "default:-m rmapbt=0"' "default is pinned to rmapbt=0"
check 'has "nosparse:-m crc=1 -i sparse=0"' "nosparse is in the list CI builds"

if [ "$fails" -eq 0 ]; then
    echo "PASS  fixture geometries have one copy"
else
    echo "FAIL  $fails check(s)" >&2
    exit 1
fi
