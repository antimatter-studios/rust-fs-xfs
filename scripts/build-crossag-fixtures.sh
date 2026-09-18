#!/usr/bin/env bash
#
# build-crossag-fixtures.sh — a file whose blocks are in more than one
# allocation group.
#
# WHY THIS IS AWKWARD TO MAKE, and why the numbers are what they are.
#
# Every other fixture here has files inside a single group, because that
# is what an allocator does when there is room. To get a file across a
# boundary it has to be bigger than the space any one group can give it,
# and a group cannot be made small: mkfs.xfs requires an internal log of
# at least 64 MB and the log lives in one group, so `-d agsize=16m` is
# refused outright.
#
# 300 MB in four groups is 75 MB each, and a 100 MB file therefore
# cannot fit in one. Measured, rather than hoped for -- the file comes
# out in two extents:
#
#     0: [0..150527]      1 (104..150631)      in group 1
#     1: [150528..204799] 3 (104..54375)       in group 3
#
# Note the groups are not even adjacent, which is the point: freeing this
# file touches two groups' headers, two sets of free-space trees and two
# reverse maps, and nothing about "the group" is singular any more.
#
# # Where this runs
#
# Anywhere with xfsprogs and the privilege to loop-mount.
# It runs INSIDE the fs-linux-test-harness guest, started by
# scripts/guest-build-fixtures.sh: one dispatcher for every set, rather
# than one wrapper per set that shipped this file into the VM.
#
#   ./scripts/build-crossag-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"

# Root inside the VM or a container, sudo on a CI runner — CARRYING PATH
# AND HOME THROUGH.
#
# `sudo` replaces PATH with its own secure_path, so an xfsprogs installed
# for this user — under ~/.local/bin, or a wrapper that finds its binary
# through $HOME — is simply not there when the command runs as root, and
# the failure reads as "command not found" from a script whose output
# nobody reads until a suite fails three steps later.
#
# That is #211 (a shutdown that never happened, swallowed by `|| true`)
# and #221 (an `xfs_bmap` that was never found, so every fixture was
# built as the same case). Both were read as driver faults first.
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo env PATH="$PATH" HOME="$HOME" "$@"
    fi
}

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# name:mkfs arguments. The reverse-mapping one is the interesting twin:
# freeing across groups has to take a record out of each group's map.
CASES=(
    "plain:-m crc=1,rmapbt=0"
    "rmap:-m crc=1,rmapbt=1"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    args="${entry#*:}"
    img="$OUT/xfscrossag-$name.img"

    rm -f "$img"
    truncate -s 300M "$img"
    # shellcheck disable=SC2086
    mkfs.xfs -f -q $args -d agcount=4 "$img" >/dev/null

    m=$(mktemp -d)
    as_root mount -o loop "$img" "$m"

    # Bigger than any one group can hold, so the allocator has to split
    # it. Written with dd rather than truncate: a sparse file has no
    # blocks to free and would make this fixture prove nothing.
    as_root dd if=/dev/zero of="$m/spanning" bs=1M count=100 status=none

    # A file inside one group, so the same test can show that the
    # ordinary case still works.
    as_root dd if=/dev/zero of="$m/withingroup" bs=4096 count=64 status=none

    sync
    as_root umount "$m"; rmdir "$m"

    groups=$(xfs_db -r -c "path /spanning" -c "bmap" "$img" 2>/dev/null | wc -l)
    echo "BUILT xfscrossag-$name ($args, spanning file in $groups extent lines)"
done

echo
echo "Cross-group fixtures in $OUT:"
ls -1 "$OUT"/xfscrossag-*.img 2>/dev/null | sed 's|.*/|  |'
