#!/usr/bin/env bash
#
# vm-build-unlink-fixtures.sh — the same filesystem before and after the
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
#   ./scripts/vm-build-unlink-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# case:files-in-fill. The victim in the root adds one more inode, so 59
# leaves the chunk exactly full and 54 leaves it with room.
CASES=(
    "spare:54"
    "wasfull:59"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    fill="${entry#*:}"
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        base=xfsunlink-$name
        rm -f \"\${base}-before.img\" \"\${base}-after.img\"
        truncate -s 400M \"\${base}-before.img\"
        mkfs.xfs -m crc=1 -f -q \"\${base}-before.img\" >/dev/null

        m=\$(mktemp -d)
        mount -o loop \"\${base}-before.img\" \"\$m\"
        mkdir \"\$m/fill\"
        i=1
        while [ \$i -le $fill ]; do : > \"\$m/fill/f\$i\"; i=\$((i + 1)); done
        : > \"\$m/victim\"
        sync
        umount \"\$m\"

        cp --reflink=never \"\${base}-before.img\" \"\${base}-after.img\"
        mount -o loop \"\${base}-after.img\" \"\$m\"
        rm -f \"\$m/victim\"
        sync
        umount \"\$m\"
        rmdir \"\$m\"

        echo \"BUILT $name (fill $fill)\"
    "
done

echo
echo "Unlink fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/xfsunlink-*.img 2>/dev/null | sed 's|.*/|  |'
