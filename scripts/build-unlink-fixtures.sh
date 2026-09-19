#!/usr/bin/env bash
#
# build-unlink-fixtures.sh — the same filesystem before and after the
# kernel removes a file, so what unlinking does to the inode B+trees can
# be read off rather than guessed.
#
# Unlinking is create in reverse, and it has the same two interesting
# cases with the interesting one the other way round:
#
#   spare     the chunk already had free inodes, so the free-inode tree
#             already held it and only the counts move
#   wasfull   the chunk had none, so giving one back puts it *into* the
#             free-inode tree — a change of membership rather than of
#             contents
#
# `wasfull` is the one worth having. A driver that updated the counts and
# left the tree alone would leave a chunk with a free inode that nothing
# can find, which is not corruption and is not detected: the filesystem
# simply loses an inode.
#
# As in the create fixtures the filler lives in a subdirectory so the
# root stays short form, since removing an entry from a root that has
# outgrown its inode rewrites a directory block instead. The victim is
# an empty file in the root: unlinking one with blocks in it also frees
# extents, which is a bigger transaction than the one measured here.
#
# # Where this runs
#
# Anywhere with xfsprogs and the privilege to loop-mount: a CI runner, a
# container, or the harness guest. scripts/guest-build-fixtures.sh is a thin
# wrapper that ships this same file into the VM, so the two cannot drift.
#
#   ./scripts/build-unlink-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

# Root inside the VM or a container, sudo on a CI runner.
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

# case:files-in-fill. The victim in the root adds one more inode, so 59
# leaves the chunk exactly full and 54 leaves it with room.
CASES=(
    "spare:54"
    "wasfull:59"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    fill="${entry#*:}"
    base="$OUT/xfsunlink-$name"

    rm -f "$base-before.img" "$base-after.img"
    truncate -s "$SIZE" "$base-before.img"
    mkfs.xfs -m crc=1,rmapbt=0 -f -q "$base-before.img" >/dev/null

    m="$(mktemp -d)"
    as_root mount -o loop "$base-before.img" "$m"
    as_root mkdir "$m/fill"
    i=1
    while [ "$i" -le "$fill" ]; do as_root touch "$m/fill/f$i"; i=$((i + 1)); done
    as_root touch "$m/victim"
    sync
    as_root umount "$m"

    cp --reflink=never "$base-before.img" "$base-after.img"
    as_root mount -o loop "$base-after.img" "$m"
    as_root rm -f "$m/victim"
    sync
    as_root umount "$m"
    rmdir "$m"

    echo "BUILT $name (fill $fill)"
done

echo
echo "Unlink fixtures in $OUT:"
ls -1 "$OUT"/xfsunlink-*.img 2>/dev/null | sed 's|.*/|  |'
