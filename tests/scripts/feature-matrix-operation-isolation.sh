#!/usr/bin/env bash
# The matrix isolates operation verdicts; the driver supports repeated checkpoints.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }

# Keep this scoped to the matrix: historical explanations in the multi-operation
# oracle correctly describe the limit that existed before the overlay (#89).
if [ ! -r "$REPO/tests/feature_matrix_oracle.rs" ]; then
    fail 'the feature-matrix source is missing or unreadable'
elif grep -Eq 'refuses a second checkpoint|ONE OPERATION PER MOUNT' \
    "$REPO/tests/feature_matrix_oracle.rs"; then
    fail 'the matrix must not explain fresh images with the obsolete checkpoint limit (#536)'
else
    ok 'the matrix does not claim a one-checkpoint mount limit'
fi

if [ "$fails" -eq 0 ]; then
    echo 'feature-matrix-operation-isolation: all checks passed'
fi
exit $(( fails > 0 ))
