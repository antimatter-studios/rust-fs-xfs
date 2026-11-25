#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

rg -Fq 'cp "$(scripts/cargo-target-dir.sh)/{{.TRIPLE}}' chores.yml || {
    echo 'FAIL  staticlib must copy from Cargo metadata target_directory' >&2
    exit 1
}

assert_target_dir() {
    local name="$1" expected="$2" actual
    shift 2
    actual="$("$REPO/scripts/cargo-target-dir.sh" "$@")"
    [[ "$actual" == "$expected" ]] || {
        printf 'FAIL  %s: expected %s, got %s\n' "$name" "$expected" "$actual" >&2
        exit 1
    }
}

unset CARGO_TARGET_DIR
assert_target_dir 'default target directory' "$REPO/target"

CARGO_TARGET_DIR="$REPO/tmp/staticlib-target-absolute"
export CARGO_TARGET_DIR
assert_target_dir 'absolute CARGO_TARGET_DIR' "$CARGO_TARGET_DIR"

CARGO_TARGET_DIR='tmp/staticlib-target-relative'
export CARGO_TARGET_DIR
assert_target_dir 'relative CARGO_TARGET_DIR' "$REPO/$CARGO_TARGET_DIR"

unset CARGO_TARGET_DIR
assert_target_dir 'absolute build.target-dir' "$REPO/tmp/staticlib-config-absolute" \
    --config "build.target-dir=\"$REPO/tmp/staticlib-config-absolute\""
assert_target_dir 'relative build.target-dir' "$REPO/tmp/staticlib-config-relative" \
    --config 'build.target-dir="tmp/staticlib-config-relative"'

printf '%s\n' 'PASS  staticlib resolves Cargo target directories from metadata'
