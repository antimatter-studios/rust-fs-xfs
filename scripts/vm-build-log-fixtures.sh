#!/usr/bin/env bash
#
# vm-build-log-fixtures.sh — build filesystems whose logs hold work.
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

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$REPO/scripts/vm.sh" up

# Block size × inode size. Not every pair is legal — a filesystem's
# inodes must fit its blocks with room to spare — and mkfs.xfs is left
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
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        img=xfslog-b${bsize}-i${isize}.img
        rm -f \"\$img\"
        truncate -s 400M \"\$img\"
        if ! mkfs.xfs -f -q -b size=$bsize -i size=$isize -m crc=1 \"\$img\" >/dev/null 2>&1; then
            rm -f \"\$img\"
            echo 'SKIP  b=$bsize i=$isize (mkfs.xfs rejected this geometry)'
            exit 0
        fi
        m=\$(mktemp -d)
        mount -o loop \"\$img\" \"\$m\"
        # Enough inodes to span more than one cluster, so the fixture
        # exercises the cluster boundary rather than only its start.
        mkdir -p \"\$m/logged\"
        for n in \$(seq 1 200); do echo \"entry \$n\" > \"\$m/logged/f\$n\"; done
        sync
        umount \"\$m\"; rmdir \"\$m\"
        echo 'BUILT b=$bsize i=$isize'
    "
done

echo
echo "Log fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/xfslog-*.img 2>/dev/null | sed 's|.*/|  |'
