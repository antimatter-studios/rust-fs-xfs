#!/usr/bin/env bash
# Every legal maintenance-mask combination needs one explicit recipe.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
for inode_trees in 0:0 1:0 1:1; do
    finobt="${inode_trees%:*}"
    inobtcount="${inode_trees#*:}"
    for rmapbt in 0 1; do
        for reflink in 0 1; do
            recipe=":-m crc=1,finobt=$finobt,inobtcount=$inobtcount,rmapbt=$rmapbt,reflink=$reflink\""
            matches="$(grep -Fc -- "$recipe" "$REPO/scripts/build-feature-matrix-fixtures.sh" || true)"
            if [ "$matches" -ne 1 ]; then
                printf 'FAIL  expected one recipe for %s; got %s\n' "$recipe" "$matches"
                fails=$((fails + 1))
            fi
        done
    done
done
for recipe in \
    'meta_uuid:-m crc=1' \
    'quota:-m crc=1' \
    'stripe:-m crc=1 -d su=64k,sw=4' \
    'sector4k:-m crc=1 -s size=4096'; do
    if ! grep -Fq -- "\"$recipe\"" "$REPO/scripts/build-feature-matrix-fixtures.sh"; then
        printf 'FAIL  missing exact recipe %s\n' "$recipe"
        fails=$((fails + 1))
    fi
done
if [ "$fails" -eq 0 ]; then
    echo 'ok    all twelve legal maintenance masks have exactly one explicit recipe'
    echo 'feature-matrix-maintenance-combinations: all checks passed'
fi
exit $(( fails > 0 ))
