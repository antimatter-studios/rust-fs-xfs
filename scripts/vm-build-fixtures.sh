#!/usr/bin/env bash
#
# vm-build-fixtures.sh — build real XFS filesystems in the oracle VM.
#
# For each geometry: create a sparse image, format it with the canonical
# mkfs.xfs, and dump superblock 0 with xfs_db. Both land in .vm-share,
# where tests/oracle_vm_fixtures.rs picks them up and requires this
# driver to agree with xfs_db field by field.
#
# Geometries are chosen to move the fields most likely to be misread:
# block and inode sizes change every log2 field, agcount changes the
# inode-number split, and the feature flags change the AG layout.
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$REPO/scripts/vm.sh" up

GEOMETRIES=(
    "default:"
    "1k:-b size=1024"
    "2k:-b size=2048"
    "i512:-i size=512"
    "i1k:-i size=1024"
    "4ags:-d agcount=4"
    "8ags:-d agcount=8"
    "reflink:-m reflink=1,rmapbt=1"
    "bigtime:-m bigtime=1"
    "nocrc:-m crc=0"
    # A v5 filesystem with sparse inodes turned off. The inode B+tree
    # record packs a hole mask and a chunk count into the four bytes a v4
    # record spends on a single free count, and without an image that has
    # v5 metadata and no sparse inodes there is no way to tell whether
    # that packing follows the format version or the feature.
    "nosparse:-m crc=1 -i sparse=0"
)

# The loop is driven from the host so the geometry list stays in one
# place and shell quoting stays sane.
for geom in "${GEOMETRIES[@]}"; do
    name="${geom%%:*}"
    args="${geom#*:}"
    "$REPO/scripts/vm.sh" run "
        set -e
        cd /share
        rm -f xfs-$name.img xfs-$name.sbdump
        truncate -s 400M xfs-$name.img
        if mkfs.xfs $args -f -q xfs-$name.img >/dev/null 2>&1; then
            xfs_db -r -c 'sb 0' -c 'print' xfs-$name.img > xfs-$name.sbdump
            # Also dump the root inode, so the inode parser is validated
            # against the reference debugger and not only against itself.
            root=\$(xfs_db -r -c 'sb 0' -c 'print rootino' xfs-$name.img | awk '{print \$3}')
            xfs_db -r -c \"inode \$root\" -c 'print' xfs-$name.img > xfs-$name.inodedump
            echo 'BUILT $name'
        else
            # Not every geometry is accepted by every xfsprogs version.
            # Skipping loudly beats a silently missing fixture.
            rm -f xfs-$name.img
            echo 'SKIP  $name (mkfs.xfs rejected this geometry)'
        fi
    "
done

echo
echo "Fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/*.img 2>/dev/null | wc -l | xargs echo "  images:"
echo
echo "Now run: cargo test --test oracle_vm_fixtures -- --nocapture"
