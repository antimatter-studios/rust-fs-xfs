#!/usr/bin/env bash
# The write-path docs claim no limit that docs/features.md says was lifted.
#
# docs/transaction-shapes.md and docs/write-support.md were written while a
# mount logged one checkpoint and the writers refused deep trees, B+tree
# forks, new inode chunks and allocations across groups. Each of those was
# lifted, docs/features.md said so, and both pages went on listing them as
# refused for several releases, because nothing read them against it (#527).
#
# Each claim below is one the pages made, in the pages' own words, beside
# what lifted it as docs/features.md and CHANGELOG.md record it. A page is
# read with its line breaks and Markdown emphasis folded away, so a claim
# rewrapped onto two lines, or emphasised, is still found.
#
#   bash tests/scripts/docs-write-limits-current.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SELF="$(basename "${BASH_SOURCE[0]}" .sh)"
fails=0
ok() { echo "ok    $*"; }
fail() { echo "FAIL  $*"; fails=$((fails + 1)); }

DOCS=(
    docs/transaction-shapes.md
    docs/write-support.md
)

# claim|what lifted it
CLAIMS=(
    'extents spanning more than one allocation group|a write placed across allocation groups, 0.13.0 (#388)'
    'extents spanning allocation groups|a write placed across allocation groups, 0.13.0 (#388)'
    'a data fork in B+tree format|truncate of a B+tree data fork, 0.8.0 (#222)'
    'free-space or inode tree more than one level deep|groups whose B+trees are more than one block deep, 0.7.0'
    'a B+tree more than one level deep|groups whose B+trees are more than one block deep, 0.7.0'
    'a group with no free inode|a new inode chunk allocated, 0.7.0 (#74)'
    'an inode chunk that needs allocating|a new inode chunk allocated, 0.7.0 (#74)'
    'already past its inode before the operation began|entries in block, leaf and node directories, 0.8.0 (#215) and 0.13.0 (#366, #424)'
    'more than one link, or is a directory, when removing it|hard links (#384) and remove_directory (#385), 0.13.0'
    'a mount writes at most one|many journalled operations in one mount, 0.8.0 (#89)'
    'the second attempt is refused|many journalled operations in one mount, 0.8.0 (#89)'
    'freeing part of a shared extent, which would split a refcount record|a partial free splits the reference-count record, 0.7.0'
    'merging an allocation into an adjacent reverse-mapping record|an allocation that continues a reverse-mapping record extends it, 0.7.0 (#75)'
)

# The page as one line: line breaks and runs of blanks become one space,
# and `*` emphasis is dropped.
flatten() {
    tr '\n\t' '  ' <"$1" | tr -d '*' | tr -s ' '
}

# Every CLAIMS entry whose claim the page at $1 still makes, one per line.
stale_claims() {
    local text entry
    text="$(flatten "$1")"
    for entry in "${CLAIMS[@]}"; do
        if grep -qiF -- "${entry%%|*}" <<<"$text"; then
            printf '%s\n' "$entry"
        fi
    done
}

mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/docs-write-limits.XXXXXX")"
trap 'rm -rf "$SANDBOX"' EXIT HUP INT TERM

# --- 1. The scan finds a claim however it is wrapped, and only claims. ----
#
# Without this a scan that matched nothing -- a pattern the flattening
# broke, a grep reading its flags differently -- would pass the real pages
# having checked nothing.
cat >"$SANDBOX/stale.md" <<'EOF'
Refused: a free-space or inode tree more than
one level deep, and a directory that was *already* past its inode
before the operation began. So a mount writes at most one.
EOF
cat >"$SANDBOX/current.md" <<'EOF'
Until 0.8.0 a mount wrote one checkpoint and refused a second. A tree
of any depth is written now.
EOF
stale="$(stale_claims "$SANDBOX/stale.md")"
current="$(stale_claims "$SANDBOX/current.md")"
found="$(grep -c . <<<"$stale")"
if [ "$found" -eq 3 ] && [ -z "$current" ]; then
    ok "the scan finds a claim across a line break and through emphasis, and nothing else"
else
    fail "the scan found $found claims in the stale page (expected 3) and '$current' in the current one (expected none):"$'\n'"$stale"
fi

# --- 2. Neither page repeats a lifted limit. ------------------------------
for doc in "${DOCS[@]}"; do
    if [ ! -f "$REPO/$doc" ]; then
        fail "$doc is missing"
        continue
    fi
    stale="$(stale_claims "$REPO/$doc")"
    if [ -z "$stale" ]; then
        ok "$doc lists no limit docs/features.md says was lifted"
        continue
    fi
    while IFS='|' read -r claim lifted; do
        fail "$doc still says \"$claim\"; lifted by $lifted"
    done <<<"$stale"
done

if [ "$fails" -eq 0 ]; then
    echo "$SELF: all checks passed"
fi
exit $(( fails > 0 ))
