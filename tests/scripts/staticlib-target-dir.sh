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

# An archive left in the default target/ by an older build must not ship. The
# copy runs with CARGO_TARGET_DIR set, so the stale file sits where the task
# used to read from, and the fresh one sits where Cargo wrote.
triple="$(rustc -vV | sed -n 's/^host: //p')"
stale="$REPO/target/$triple/release/libfs_xfs.a"
fresh="$REPO/tmp/staticlib-stale-target/$triple/release/libfs_xfs.a"
out="$REPO/tmp/staticlib-stale-dist/libfs_xfs.a"
stale_backup=''
if [[ -e "$stale" ]]; then
    stale_backup="$(mktemp)"
    cp -p "$stale" "$stale_backup"
fi
restore_stale() {
    if [[ -n "$stale_backup" ]]; then
        mv "$stale_backup" "$stale"
    else
        rm -f "$stale"
    fi
}
trap restore_stale EXIT

mkdir -p "$(dirname "$stale")" "$(dirname "$fresh")" "$(dirname "$out")"
printf 'stale\n' > "$stale"
printf 'fresh\n' > "$fresh"
CARGO_TARGET_DIR="$REPO/tmp/staticlib-stale-target"
export CARGO_TARGET_DIR
# The same copy source chores.yml runs for `chore staticlib`.
cp "$("$REPO/scripts/cargo-target-dir.sh")/$triple/release/libfs_xfs.a" "$out"
cmp -s "$out" "$fresh" || {
    echo 'FAIL  stale: shipped archive differs from Cargo output' >&2
    exit 1
}

printf '%s\n' 'PASS  staticlib resolves Cargo target directories from metadata'
