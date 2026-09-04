#!/usr/bin/env bash
#
# build-log-fixtures.sh — build filesystems whose logs hold work.
#
# The geometry fixtures from vm-build-fixtures.sh are formatted and never
# mounted, so their logs contain one unmount record and nothing else.
# That is the right shape for checking the superblock parser and the
# wrong one for checking anything about the log: a log with no items in
# it cannot disagree with us about how an item is written.
#
# These are mounted, written to, and unmounted. The records stay in the
# ring afterwards — a clean unmount adds a record, it does not erase the
# ones before it — so each image carries a few thousand real log items
# for tests to read.
#
# The inode size is the point. A logged inode addresses the *cluster*
# holding it, and the cluster's size scales with the inode size by a rule
# that is not in the record and not obvious. One inode size proves the
# arithmetic and nothing about the rule, so this varies it as far as
# mkfs.xfs will allow.
#
#   ./scripts/vm-build-log-fixtures.sh
set -euo pipefail

# WHERE THIS RUNS. Anywhere with xfsprogs and the privilege to
# loop-mount: a CI runner, a container, or the oracle VM.
# `vm-build-log-fixtures.sh` ships this same file into the VM, so the two
# cannot drift.

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# Block size x inode size. Not every pair is legal -- a filesystem's
# inodes must fit its blocks with room to spare -- and mkfs.xfs is left
# to say which, rather than this script encoding a rule that changes
# between versions.
GEOMETRIES=(
    "1024 512"
    "4096 512"
    "4096 1024"
    "4096 2048"
)

for geom in "${GEOMETRIES[@]}"; do
    read -r bsize isize <<<"$geom"
    img="$OUT/xfslog-b${bsize}-i${isize}.img"

    rm -f "$img"
    truncate -s "$SIZE" "$img"
    # rmapbt=0: these are the images the write oracles mount read-write,
    # and this driver refuses a reverse-mapping filesystem because it
    # does not maintain the tree. Modern mkfs.xfs turns it on by default.
    if ! mkfs.xfs -f -q -b "size=$bsize" -i "size=$isize" -m crc=1,rmapbt=0 "$img" >/dev/null 2>&1; then
        rm -f "$img"
        echo "SKIP  b=$bsize i=$isize (mkfs.xfs rejected this geometry)"
        continue
    fi

    m=$(mktemp -d)
    $SUDO mount -o loop "$img" "$m"
    # Enough inodes to span more than one cluster, so the fixture
    # exercises the cluster boundary rather than only its start.
    $SUDO mkdir -p "$m/logged"
    for n in $(seq 1 200); do echo "entry $n" | $SUDO tee "$m/logged/f$n" > /dev/null; done
    # A directory small enough to stay inside its inode, for the tests
    # that rewrite a short-form directory. Two entries of equal name
    # length, so a rename between them changes nothing but the name and
    # cannot be passed by accident.
    $SUDO mkdir -p "$m/sf"
    echo one | $SUDO tee "$m/sf/aaaa" > /dev/null
    echo two | $SUDO tee "$m/sf/bbbb" > /dev/null
    sync
    $SUDO umount "$m"; rmdir "$m"
    echo "BUILT b=$bsize i=$isize"
done

echo
echo "Log fixtures in $OUT:"
ls -1 "$OUT"/xfslog-*.img 2>/dev/null | sed 's|.*/|  |'
