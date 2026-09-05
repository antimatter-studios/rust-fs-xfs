#!/usr/bin/env bash
#
# build-feature-matrix-fixtures.sh — one populated filesystem per legal
# combination of the features that change what a WRITE has to maintain.
#
# WHY THIS EXISTS. Every write fixture in this repository was formatted
# one way, so every write test had only ever exercised one feature set.
# That is how `rmapbt` went unnoticed: mkfs.xfs 6.6 turns it on by
# default, the oracle VM's older one does not, and the first CI run on a
# modern runner produced a filesystem xfs_repair called broken.
#
# A driver is correct on a filesystem or it is not, and which features
# the filesystem has is not the driver's choice. So the combinations are
# enumerated rather than sampled, and every one of them is written to.
#
# # The axes, and why these
#
# Each adds a structure that an allocation or a free has to keep in step.
# A driver that ignores one does not fail: it leaves a filesystem that
# mounts, behaves, and disagrees with xfs_repair.
#
#   finobt      a second inode B+tree holding only chunks with free
#               inodes. Creating and unlinking move chunks in and out.
#   inobtcount  block counts for those trees, kept in the AGI. Requires
#               finobt; mkfs refuses it without.
#   rmapbt      a reverse map, owner per extent. EVERY allocation and
#               free has to add or remove a record.
#   reflink     a refcount tree for shared extents. Freeing an extent
#               that another file shares must decrement rather than
#               free, so a driver that cannot tell must not free.
#
# bigtime and nrext64 change how an inode is ENCODED rather than what a
# write must maintain, and the read oracles already vary them. crc=0 (v4)
# takes none of the four, so it is one row rather than an axis.
#
# # What the answer is
#
# Not this script's business. It builds the filesystems; the kernel
# replays what the driver logged and xfs_repair says whether the result
# is sound. See tests/feature_matrix_oracle.rs.
#
#   ./scripts/build-feature-matrix-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# name:mkfs -m arguments. `inobtcount` only appears with `finobt=1`,
# which is the one dependency mkfs enforces.
COMBOS=(
    "v4:crc=0"
    "base:crc=1,finobt=0,rmapbt=0,reflink=0"
    "finobt:crc=1,finobt=1,rmapbt=0,reflink=0"
    "finobt-inobtcount:crc=1,finobt=1,inobtcount=1,rmapbt=0,reflink=0"
    "reflink:crc=1,finobt=0,rmapbt=0,reflink=1"
    "reflink-finobt:crc=1,finobt=1,rmapbt=0,reflink=1"
    "rmapbt:crc=1,finobt=0,rmapbt=1,reflink=0"
    "rmapbt-reflink:crc=1,finobt=1,rmapbt=1,reflink=1"
    "everything:crc=1,finobt=1,inobtcount=1,rmapbt=1,reflink=1"
)

built=0
for combo in "${COMBOS[@]}"; do
    name="${combo%%:*}"
    args="${combo#*:}"
    img="$OUT/xfsfeat-$name.img"

    rm -f "$img"
    truncate -s "$SIZE" "$img"
    if ! mkfs.xfs -f -q -m "$args" "$img" >/dev/null 2>&1; then
        rm -f "$img"
        echo "SKIP  $name (mkfs.xfs rejected -m $args)"
        continue
    fi

    m=$(mktemp -d)
    $SUDO mount -o loop "$img" "$m"

    # THE SAME TREE ON EVERY ROW, so a difference in the result is a
    # difference in the features and nothing else. Each entry exists for
    # one write operation in tests/feature_matrix_oracle.rs.
    #
    # The directory stays short form -- two entries of equal name length
    # -- because a rename inside one rewrites the inode's own fork, and a
    # rename in a block-form directory is a different operation this
    # driver refuses by name.
    $SUDO mkdir -p "$m/sf"
    echo one | $SUDO tee "$m/sf/aaaa" > /dev/null   # renamed
    echo two | $SUDO tee "$m/sf/bbbb" > /dev/null   # left alone, must survive
    $SUDO dd if=/dev/urandom of="$m/sf/data.bin" bs=4096 count=32 status=none  # truncated
    $SUDO touch "$m/sf/empty.bin"                   # written into
    $SUDO touch "$m/sf/victim"                      # unlinked
    $SUDO touch "$m/sf/attrs"                       # chmod/utimes

    # A SHARED EXTENT, where the filesystem allows one.
    #
    # `reflink=1` on its own only means sharing is permitted. The case
    # that can actually go wrong is an extent two files point at: freeing
    # it must decrement the refcount rather than return the blocks, and a
    # driver that cannot tell the difference would hand out blocks
    # another file is still using. A fixture with the feature enabled and
    # nothing shared does not test that at all.
    if $SUDO cp --reflink=always "$m/sf/data.bin" "$m/sf/shared.bin" 2>/dev/null; then
        echo "  (shared extent created: sf/shared.bin reflinks sf/data.bin)"
    else
        echo "  (no reflink support here; sf/shared.bin not created)"
    fi

    # Enough inodes that the group's chunk is neither fresh nor full, so
    # creating and unlinking move real records rather than the first and
    # only ones.
    $SUDO mkdir -p "$m/fill"
    for n in $(seq 1 40); do $SUDO touch "$m/fill/f$n"; done

    sync
    # Unmounted rather than only synced: a mounted filesystem's headers
    # are a cache of what is in memory, and the tests read the image.
    $SUDO umount "$m"; rmdir "$m"

    echo "BUILT xfsfeat-$name  (-m $args)"
    built=$((built + 1))
done

echo
echo "$built feature-matrix fixtures in $OUT:"
ls -1 "$OUT"/xfsfeat-*.img 2>/dev/null | sed 's|.*/|  |'
