#!/usr/bin/env bash
#
# build-truncate-fixtures.sh — the same filesystem before and after a
# truncate, so what the kernel did to the free-space trees can be read
# off rather than guessed.
#
# Freeing an extent is the part of truncate that cannot be checked by
# looking at the result: a file whose size is zero looks the same whether
# its blocks went back to the allocation group correctly, went back
# twice, or did not go back at all. The difference is entirely in the two
# free-space trees and the group header.
#
# So each case is captured twice — once with the file whole and once
# after the kernel truncated it — and the pair is the oracle. A driver
# that can predict the second image's trees from the first has understood
# what freeing an extent means; one that cannot has not, however plausible
# the trees it produces look on their own.
#
# # The cases, and why they are chosen by measurement
#
# What the freed extent is next to is what decides whether a record is
# inserted, widened, or removed entirely:
#
#   lone      free space on neither side — a new record in both trees
#   after     free space immediately after it  — one record grows down
#   before    free space immediately before it — one record grows up
#   between   free space on both sides — two records collapse into one
#
# Which neighbour to remove is found by asking the filesystem where each
# file actually landed, rather than by assuming files are laid out in the
# order they were created. They are not, reliably, and an earlier version
# of this script built the same case four times without saying so.
#
# # Where this runs
#
# Anywhere with xfsprogs and the privilege to loop-mount: a CI runner, a
# container, or the harness guest. scripts/guest-build-fixtures.sh is a thin
# wrapper that ships this same file into the VM and runs it there, so the
# two cannot drift — the geometry fixtures had two builders that did.
#
#   ./scripts/build-truncate-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

# One megabyte per file: enough to be a real extent, small enough that
# the group's free space stays in a single-level tree.
FILE_MB="${XFS_FIXTURE_FILE_MB:-1}"

# Root inside the VM or a container, sudo on a CI runner.
#
# CARRYING PATH AND HOME THROUGH, because `sudo` replaces PATH with its
# own secure_path: an xfsprogs installed for this user -- under
# ~/.local/bin, or a wrapper that finds its binary through $HOME -- is
# simply not there when the command runs as root.
#
# That is what made this script produce fixtures that were not what they
# said they were (#221). `xfs_bmap` ran under plain `sudo`, was not
# found, and `span` came back empty; every neighbour then read as
# "none", the removals below were skipped, and all four cases were built
# as `lone`. The suite said so three steps later, as a driver fault.
# Same defect as #211, in the next script along.
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo env PATH="$PATH" HOME="$HOME" "$@"
    fi
}

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }
command -v xfs_bmap >/dev/null || { echo "xfs_bmap not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# The first extent's device range, in 512-byte basic blocks. The third
# line of `xfs_bmap -v` is the first extent; its third field is the range.
span() {
    local out
    out="$(as_root xfs_bmap -v "$1" | sed -n 3p | awk '{print $3}')"
    # `a..b`, in basic blocks, or this script has no idea where the file
    # is and every case below is built by accident.
    case "$out" in
        *..*) echo "$out" ;;
        *) echo "xfs_bmap did not report an extent for $1: '$out'" >&2; exit 1 ;;
    esac
}
first() { echo "$1" | awk -F'\\.\\.' '{print $1}'; }
last()  { echo "$1" | awk -F'\\.\\.' '{print $2}'; }

for CASE in lone after before between; do
    base="$OUT/xfstrunc-$CASE"
    rm -f "$base-before.img" "$base-after.img"
    truncate -s "$SIZE" "$base-before.img"
    mkfs.xfs -f -q -m crc=1,rmapbt=0 "$base-before.img" >/dev/null

    m="$(mktemp -d)"
    as_root mount -o loop "$base-before.img" "$m"

    # A row of files, the middle one of which will be truncated. Created
    # in place rather than renamed into place: a rename after the fact
    # leaves the row in a different order than it reads.
    for n in f1 f2 victim f4 f5; do
        as_root dd if=/dev/zero of="$m/$n" bs=1M count="$FILE_MB" status=none
    done
    sync

    victim_start="$(first "$(span "$m/victim")")"
    victim_end="$(last  "$(span "$m/victim")")"
    next=""; prev=""
    for n in f1 f2 f4 f5; do
        s="$(first "$(span "$m/$n")")"
        e="$(last  "$(span "$m/$n")")"
        [ "$s" = "$((victim_end + 1))" ] && next="$n"
        [ "$e" = "$((victim_start - 1))" ] && prev="$n"
    done

    # Remove whichever neighbours this case wants gone, so the victim's
    # blocks are freed next to free space, or not, as named.
    #
    # A MISSING NEIGHBOUR IS A FAILED BUILD, not a quieter fixture. The
    # allocator decides where these files land, and a kernel that puts
    # them somewhere else leaves `after` with nothing to merge with --
    # so it is built as `lone`, passes its own build, and fails the
    # suite later as though the driver had changed (#221).
    need() {
        [ -n "$2" ] && return 0
        echo "the $CASE fixture needs a file immediately $1 the victim, and this" >&2
        echo "kernel put none there: victim at $victim_start..$victim_end." >&2
        echo "The neighbours it did place:" >&2
        for n in f1 f2 f4 f5; do
            echo "  $n $(span "$m/$n")" >&2
        done
        exit 1
    }
    case "$CASE" in
        lone)    ;;
        after)   need after "$next";  as_root rm -f "$m/$next" ;;
        before)  need before "$prev"; as_root rm -f "$m/$prev" ;;
        between) need after "$next";  need before "$prev"
                 as_root rm -f "$m/$next"
                 as_root rm -f "$m/$prev" ;;
    esac
    sync
    # Unmounted rather than only synced: a mounted filesystem's group
    # header is a cache of what is in memory, and the tests read the
    # image rather than the mount.
    as_root umount "$m"

    cp --reflink=never "$base-before.img" "$base-after.img"
    as_root mount -o loop "$base-after.img" "$m"
    as_root truncate -s 0 "$m/victim"
    sync
    as_root umount "$m"
    rmdir "$m"

    echo "BUILT $CASE  victim at $victim_start..$victim_end, neighbour before=${prev:-none} after=${next:-none}"
done

echo
echo "Truncate fixtures in $OUT:"
ls -1 "$OUT"/xfstrunc-*.img 2>/dev/null | sed 's|.*/|  |'
