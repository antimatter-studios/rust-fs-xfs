#!/usr/bin/env bash
#
# fixture-geometries-single-copy.sh — the fixture builder reads the one
# geometry list (#110).
#
# There were two builders, one for a CI runner and one for the VM, and
# each carried a GEOMETRIES array of its own. They drifted: CI never
# built `nosparse`, and the VM left `default` unpinned, so CI and a
# developer's local run stopped meaning the same thing. There is one
# builder now — scripts/guest-build-fixtures.sh, in the harness guest, on
# every machine — and it sources scripts/fixture-geometries.sh. This
# fails if it grows a list of its own again or stops sourcing the shared
# one, and pins the two entries whose drift was the defect.
#
# THE CHECK OUTLIVES ITS SECOND BUILDER ON PURPOSE. The defect was a list
# copied, not a list copied twice, and the next builder to want these
# geometries will be written by somebody who has not read #110.
#
#   bash tests/scripts/fixture-geometries-single-copy.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }

for builder in guest-build-fixtures.sh; do
    f="$REPO/scripts/$builder"
    if grep -qF 'source "$REPO/scripts/fixture-geometries.sh"' "$f"; then
        ok "$builder sources the shared geometry list"
    else
        fail "$builder sources the shared geometry list"
    fi
    # Any assignment to a geometry array: a declaration, an append with
    # `+=`, or one element replaced by index. Each would make this builder's
    # set differ from the shared one again.
    if grep -qE '^[[:space:]]*[A-Za-z_]*GEOMETRIES(\[[^]]*\])?\+?=' "$f"; then
        fail "$builder declares or changes no geometry list of its own"
    else
        ok "$builder declares or changes no geometry list of its own"
    fi
    if grep -qF '"${XFS_GEOMETRIES[@]}"' "$f"; then
        ok "$builder builds from that list"
    else
        fail "$builder builds from that list"
    fi
done

# shellcheck source=/dev/null
source "$REPO/scripts/fixture-geometries.sh"
has() {
    local g
    for g in "${XFS_GEOMETRIES[@]}"; do [ "$g" = "$1" ] && return 0; done
    return 1
}
if has "default:-m rmapbt=0"; then ok "default is pinned to rmapbt=0"; else fail "default is pinned to rmapbt=0"; fi
if has "nosparse:-m crc=1 -i sparse=0"; then ok "nosparse is in the list CI builds"; else fail "nosparse is in the list CI builds"; fi

# The mutation check itself, against the shapes it must catch.
probe="$(mktemp)"
trap 'rm -f "$probe"' EXIT
for shape in 'GEOMETRIES=(' 'XFS_GEOMETRIES+=("extra:-b size=512")' 'XFS_GEOMETRIES[0]="default:"'; do
    printf '%s\n' "  $shape" > "$probe"
    if grep -qE '^[[:space:]]*[A-Za-z_]*GEOMETRIES(\[[^]]*\])?\+?=' "$probe"; then
        ok "the check catches: $shape"
    else
        fail "the check catches: $shape"
    fi
done

if [ "$fails" -eq 0 ]; then
    echo "PASS  fixture geometries have one copy"
else
    echo "FAIL  $fails check(s)" >&2
    exit 1
fi
