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

# name:mkfs arguments. `inobtcount` only appears with `finobt=1`, which
# is the one dependency mkfs enforces.
#
# TWO GROUPS OF ROWS, and they are asking different questions.
#
# The first vary what a write must MAINTAIN -- finobt, inobtcount,
# rmapbt, reflink. Each adds a structure that an allocation or a free has
# to keep in step, and a driver that ignores one leaves a filesystem that
# mounts, behaves, and disagrees with xfs_repair.
#
# The second vary how an inode or a directory is ENCODED -- bigtime,
# nrext64, sparse inodes, and the block, inode and directory-block sizes.
# Nothing extra has to be maintained; the same fields simply live
# somewhere else or are a different width. The read oracles vary some of
# these and nothing varied them against the WRITE path, which is a gap of
# the same shape as the one that hid rmapbt: every write fixture happened
# to be formatted one way.
#
# ftype is not an axis. mkfs refuses `-n ftype=0` on v5 and requires it
# on v4, so it is not independently selectable -- the v4 row carries the
# only ftype=0 filesystem there can be.
COMBOS=(
    "v4:-m crc=0 -n ftype=0"
    "base:-m crc=1,finobt=0,rmapbt=0,reflink=0"
    "finobt:-m crc=1,finobt=1,rmapbt=0,reflink=0"
    "finobt-inobtcount:-m crc=1,finobt=1,inobtcount=1,rmapbt=0,reflink=0"
    "reflink:-m crc=1,finobt=0,rmapbt=0,reflink=1"
    "reflink-finobt:-m crc=1,finobt=1,rmapbt=0,reflink=1"
    "rmapbt:-m crc=1,finobt=0,rmapbt=1,reflink=0"
    "rmapbt-reflink:-m crc=1,finobt=1,rmapbt=1,reflink=1"
    "everything:-m crc=1,finobt=1,inobtcount=1,rmapbt=1,reflink=1"

    # --- how things are encoded, with the features held still ---------
    #
    # Timestamps as a 32-bit second/nanosecond pair rather than one
    # 64-bit count. field_layout has an arm for this that no fixture
    # reached until recently.
    "bigtime0:-m crc=1,bigtime=0"
    # 64-bit extent counts. di_nextents moves to offset 24 and the
    # fields around it shift, so every inode this driver writes is laid
    # out differently.
    "nrext64:-m crc=1 -i nrext64=1"
    "nrext64-bigtime0:-m crc=1,bigtime=0 -i nrext64=1"
    # Sparse inode chunks: the inode B+tree record grows a hole mask and
    # a count, which is a different record shape in the same tree.
    "sparse:-m crc=1 -i sparse=1"
    "nosparse:-m crc=1 -i sparse=0"
    # Block size. Every log2-derived field moves with it, and a smaller
    # block means a smaller tree root and fewer records before a split.
    "b1k:-m crc=1 -b size=1024"
    "b2k:-m crc=1 -b size=2048"
    # Inode size, which decides how much of an inode its forks get.
    "i1k:-m crc=1 -i size=1024"
    # A directory block larger than a filesystem block, so a directory
    # that leaves its inode needs several blocks rather than one.
    "dirblock8k:-m crc=1 -b size=4096 -n size=8192"
)

built=0
for combo in "${COMBOS[@]}"; do
    name="${combo%%:*}"
    args="${combo#*:}"
    img="$OUT/xfsfeat-$name.img"

    rm -f "$img"
    truncate -s "$SIZE" "$img"
    # shellcheck disable=SC2086
    if ! mkfs.xfs -f -q $args "$img" >/dev/null 2>&1; then
        rm -f "$img"
        echo "SKIP  $name (mkfs.xfs rejected $args)"
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

    # A DIRECTORY ONE ENTRY SHORT OF LEAVING ITS INODE.
    #
    # Adding to it is the only operation here that ALLOCATES: the
    # directory moves into a block of its own, which has to come out of
    # free space and be recorded in the reverse map. Without this the
    # matrix never allocates for a directory at all, and the row that
    # cannot do it -- a directory block larger than a filesystem block --
    # is never asked.
    #
    # The count is found by filling until the kernel converts and
    # stepping back one, because how many entries fit depends on the
    # inode size, the name length and whether the directory carries an
    # attribute fork. The same measurement build-dirconv-fixtures.sh
    # makes, for the same reason: assuming a number here produced a test
    # that failed on a runner where it was 17 rather than 30.
    $SUDO mkdir -p "$m/full"
    n=0
    while [ "$n" -lt 400 ]; do
        $SUDO touch "$m/full/e$n"
        $SUDO sync
        [ "$($SUDO stat -c %b "$m/full")" != "0" ] && break
        n=$((n + 1))
    done
    if [ "$n" -ge 400 ]; then
        echo "FAILED $name — 400 entries did not overflow the inode" >&2
        $SUDO umount "$m"; rmdir "$m"; exit 1
    fi
    $SUDO rm -f "$m/full/e$n"
    $SUDO sync

    # Enough inodes that the group's chunk is neither fresh nor full, so
    # creating and unlinking move real records rather than the first and
    # only ones.
    $SUDO mkdir -p "$m/fill"
    for n in $(seq 1 40); do $SUDO touch "$m/fill/f$n"; done

    sync
    # Unmounted rather than only synced: a mounted filesystem's headers
    # are a cache of what is in memory, and the tests read the image.
    $SUDO umount "$m"; rmdir "$m"

    echo "BUILT xfsfeat-$name  ($args)"
    built=$((built + 1))
done

echo
echo "$built feature-matrix fixtures in $OUT:"
ls -1 "$OUT"/xfsfeat-*.img 2>/dev/null | sed 's|.*/|  |'
