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
#   ./scripts/vm-build-create-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# case:files-before
CASES=(
    "spare:55"
    "last:59"
    "newchunk:60"
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
        mkdir \"\$m/fill\"
        i=1
        while [ \$i -le $fill ]; do : > \"\$m/fill/f\$i\"; i=\$((i + 1)); done
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
