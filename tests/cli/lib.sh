# tests/cli/lib.sh — what every tests/cli/test-*.sh sources.
#
# The house style: `ok`/`fail`, a `fails` counter, `set -uo pipefail` (not
# -e, so later checks still run after one fails), a sandbox under the
# repository's tmp/, and `finish` last, which prints the count
# `scripts/ci-test.sh --gate` reads and the trailing `<name>: all checks
# passed` line scripts/test-cli.sh requires.
#
# The tools are whatever PATH finds: scripts/test-cli.sh has already made
# sure they are ours, and that the images are there, before any file runs.
set -uo pipefail

NAME="$(basename "$0" .sh)"
REPO="$(cd "$(dirname "$0")/../.." && pwd -P)"
CRATE="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"
SHARE="$REPO/.vm-share"

passed=0
fails=0
ok() { passed=$((passed + 1)); }
fail() {
    echo "FAIL  $NAME: $*" >&2
    fails=$((fails + 1))
}

# check DESCRIPTION COMMAND...: ok if COMMAND succeeds, else fail naming it.
check() {
    local what="$1"
    shift
    if "$@"; then ok; else fail "$what"; fi
}

# jq_check DESCRIPTION FILTER FILE: ok if jq -e FILTER holds for FILE.
jq_check() {
    local what="$1" filter="$2" file="$3"
    if jq -e "$filter" "$file" >/dev/null 2>&1; then
        ok
    else
        fail "$what: jq -e '$filter' is false for: $(head -c 2000 "$file")"
    fi
}

# need_fixture NAME...: every named kernel-made image or manifest is in
# .vm-share, or this file fails at once naming the task that builds them.
# Never a skip. (scripts/test-cli.sh checks the images before any file
# runs; this is the same rule for a file run on its own.)
need_fixture() {
    local name
    for name in "$@"; do
        if [ ! -f "$SHARE/$name" ]; then
            fail ".vm-share/$name is missing; build it with \`chore fixtures -- cli\`"
            finish
        fi
    done
}

# copy_image SOURCE DEST: a private copy to write to, never the fixture
# itself. A clone where the filesystem has them (APFS), holes kept where
# it does not: the images are 320 MiB and almost all of it is holes.
copy_image() {
    cp -c "$1" "$2" 2>/dev/null || cp --sparse=always "$1" "$2" 2>/dev/null || cp "$1" "$2"
}

# poke FILE OFFSET: invert the byte at OFFSET, in place.
poke() {
    local file="$1" at="$2" byte
    byte="$(od -An -tu1 -j "$at" -N1 "$file" | tr -d ' ')"
    printf "\\$(printf '%03o' $((255 - byte)))" |
        dd of="$file" bs=1 seek="$at" conv=notrunc status=none
}

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/cli-$NAME.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

finish() {
    if [ "$fails" -gt 0 ]; then
        echo "test result: FAILED. $passed passed; $fails failed"
        exit 1
    fi
    echo "test result: ok. $passed passed; 0 failed"
    echo "$NAME: all checks passed"
    exit 0
}
