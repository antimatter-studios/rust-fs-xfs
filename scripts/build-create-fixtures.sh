#!/usr/bin/env bash
#
# build-create-fixtures.sh — the same filesystem before and after the
# kernel creates a file, so what creating one does to the inode B+trees
# can be read off rather than guessed.
#
# Inodes are allocated in chunks of 64, and which of three things happens
# depends on how full the chunk the group is using happens to be. A
# fixture that only ever creates a file on a fresh filesystem exercises
# one of them and gives no sign that the others exist:
#
#   spare      the chunk has several free inodes; it keeps its place in
#              the free-inode tree
#   last       the chunk's last free inode is taken, so it leaves the
#              free-inode tree entirely
#   newchunk   no chunk has a free inode, so a whole new one is
#              allocated — which also allocates blocks, and is refused by
#              name rather than attempted
#
# The filler files go in a subdirectory rather than in the root, and that
# is not tidiness. A root holding 55 files has outgrown its inode, and
# adding an entry to it means rewriting a directory block rather than the
# inode's own fork — so a fixture built that way can exercise the inode
# accounting but not the transaction that depends on it. With the filler
# in `fill/`, the root holds one entry and stays short form, which is
# what lets the same images serve both.
#
# The subdirectory lands in the same allocation group as the root, so it
# still consumes that group's inodes. That was measured rather than
# assumed — it is not true of every geometry.
#
# The fill levels are not guessed either. A freshly formatted 400 MB
# filesystem has one chunk of 64 inodes, and with the root, the
# subdirectory and the kernel's own metadata inodes accounted for, 55
# leaves five free, 59 leaves exactly one and 60 leaves none. Those were
# measured by building images at several levels and reading the group's
# inode header back; if a future mkfs.xfs uses a different number of
# inodes they will need measuring again, and the test asserts which case
# each fixture produced so that a drift shows up as a failure rather than
# as silent loss of coverage.
#
# # Where this runs
#
# Anywhere with xfsprogs and the privilege to loop-mount: a CI runner, a
# container, or the oracle VM. `vm-build-create-fixtures.sh` is a thin
# wrapper that ships this same file into the VM, so the two cannot drift.
#
#   ./scripts/build-create-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

# Root inside the VM or a container, sudo on a CI runner.
SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# case:files-before
CASES=(
    "spare:55"
    "last:59"
    "newchunk:60"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    fill="${entry#*:}"
    base="$OUT/xfscreate-$name"

    rm -f "$base-before.img" "$base-after.img"
    truncate -s "$SIZE" "$base-before.img"
    mkfs.xfs -m crc=1 -f -q "$base-before.img" >/dev/null

    m="$(mktemp -d)"
    $SUDO mount -o loop "$base-before.img" "$m"
    $SUDO mkdir "$m/fill"
    i=1
    while [ "$i" -le "$fill" ]; do $SUDO touch "$m/fill/f$i"; i=$((i + 1)); done
    sync
    # Unmounted rather than only synced: a mounted filesystem's group
    # header is a cache of what is in memory, and the tests read the
    # image rather than the mount.
    $SUDO umount "$m"

    cp --reflink=never "$base-before.img" "$base-after.img"
    $SUDO mount -o loop "$base-after.img" "$m"
    $SUDO touch "$m/victim"
    sync
    $SUDO umount "$m"
    rmdir "$m"

    echo "BUILT $name (after $fill files)"
done

echo
echo "Create fixtures in $OUT:"
ls -1 "$OUT"/xfscreate-*.img 2>/dev/null | sed 's|.*/|  |'
