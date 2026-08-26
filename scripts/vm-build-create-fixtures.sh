#!/usr/bin/env bash
#
# vm-build-create-fixtures.sh — the same filesystem before and after the
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
# The fill levels are not guessed. A freshly formatted 400 MB filesystem
# has one chunk of 64 inodes with three already in use, so 60 files
# leaves exactly one free and 61 leaves none. Those numbers were measured
# by building images at several fill levels and reading the group's inode
# header back; if a future mkfs.xfs uses a different number of inodes for
# its own metadata they will need measuring again, and the test asserts
# which case each fixture produced so that a drift shows up as a failure
# rather than as silent loss of coverage.
#
#   ./scripts/vm-build-create-fixtures.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# case:files-before
CASES=(
    "spare:55"
    "last:60"
    "newchunk:61"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    fill="${entry#*:}"
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        base=xfscreate-$name
        rm -f \"\${base}-before.img\" \"\${base}-after.img\"
        truncate -s 400M \"\${base}-before.img\"
        mkfs.xfs -m crc=1 -f -q \"\${base}-before.img\" >/dev/null

        m=\$(mktemp -d)
        mount -o loop \"\${base}-before.img\" \"\$m\"
        i=1
        while [ \$i -le $fill ]; do : > \"\$m/f\$i\"; i=\$((i + 1)); done
        sync
        # Unmounted rather than only synced: a mounted filesystem's group
        # header is a cache of what is in memory, and the tests read the
        # image rather than the mount.
        umount \"\$m\"

        cp --reflink=never \"\${base}-before.img\" \"\${base}-after.img\"
        mount -o loop \"\${base}-after.img\" \"\$m\"
        : > \"\$m/victim\"
        sync
        umount \"\$m\"
        rmdir \"\$m\"

        echo \"BUILT $name (after $fill files)\"
    "
done

echo
echo "Create fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/xfscreate-*.img 2>/dev/null | sed 's|.*/|  |'
