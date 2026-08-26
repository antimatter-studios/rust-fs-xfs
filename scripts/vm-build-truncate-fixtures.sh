#!/usr/bin/env bash
#
# vm-build-truncate-fixtures.sh — the same filesystem before and after a
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
# order they were created. They usually are — but "usually" is how a
# fixture ends up silently building the same case four times, which is
# exactly what an earlier version of this script did.
#
#   ./scripts/vm-build-truncate-fixtures.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# One megabyte per file: enough to be a real extent, small enough that
# the group's free space stays in a single-level tree.
FILE_MB=1

for CASE in lone after before between; do
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        base=xfstrunc-$CASE
        rm -f \"\${base}-before.img\" \"\${base}-after.img\"
        truncate -s 400M \"\${base}-before.img\"
        mkfs.xfs -f -q -m crc=1 \"\${base}-before.img\" >/dev/null

        m=\$(mktemp -d)
        mount -o loop \"\${base}-before.img\" \"\$m\"

        # A row of files, the middle one of which will be truncated.
        # Created in place rather than renamed into place: a rename
        # after the fact leaves the row in a different order than it
        # reads, which is how an earlier version of this script built
        # the same case four times without saying so.
        for n in f1 f2 victim f4 f5; do
            dd if=/dev/zero of=\"\$m/\$n\" bs=1M count=$FILE_MB status=none
        done
        sync

        # Where each file actually is, in 512-byte basic blocks. The
        # third line of xfs_bmap -v is the first extent; its third field
        # is the device range.
        span() { xfs_bmap -v \"\$1\" | sed -n 3p | awk '{print \$3}'; }
        first() { echo \"\$1\" | awk -F'\.\.' '{print \$1}'; }
        last()  { echo \"\$1\" | awk -F'\.\.' '{print \$2}'; }

        vs=\$(first \"\$(span \"\$m/victim\")\")
        ve=\$(last  \"\$(span \"\$m/victim\")\")
        next=''; prev=''
        for n in f1 f2 f4 f5; do
            s=\$(first \"\$(span \"\$m/\$n\")\")
            e=\$(last  \"\$(span \"\$m/\$n\")\")
            [ \"\$s\" = \"\$((ve + 1))\" ] && next=\"\$n\"
            [ \"\$e\" = \"\$((vs - 1))\" ] && prev=\"\$n\"
        done

        case '$CASE' in
            lone)    ;;
            after)   [ -n \"\$next\" ] && rm -f \"\$m/\$next\" ;;
            before)  [ -n \"\$prev\" ] && rm -f \"\$m/\$prev\" ;;
            between) [ -n \"\$next\" ] && rm -f \"\$m/\$next\"
                     [ -n \"\$prev\" ] && rm -f \"\$m/\$prev\" ;;
        esac
        sync
        # Unmounted rather than only synced: a mounted filesystem's group
        # header is a cache of what is in memory, and the tests read the
        # image rather than the mount.
        umount \"\$m\"

        cp --reflink=never \"\${base}-before.img\" \"\${base}-after.img\"
        mount -o loop \"\${base}-after.img\" \"\$m\"
        truncate -s 0 \"\$m/victim\"
        sync
        umount \"\$m\"
        rmdir \"\$m\"

        echo \"BUILT $CASE  victim at \$vs..\$ve, neighbour before=\${prev:-none} after=\${next:-none}\"
    "
done

echo
echo "Truncate fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/xfstrunc-*.img 2>/dev/null | sed 's|.*/|  |'
