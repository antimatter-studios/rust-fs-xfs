#!/usr/bin/env bash
#
# build-deeptree-fixtures.sh — filesystems whose group trees are more
# than one block deep.
#
# Every other fixture here has single-level trees, because a fresh
# filesystem has one free extent and a handful of inodes, and one block
# holds those with room to spare. That is the shape the write paths were
# built against, and it is not the shape of a filesystem anyone uses:
# fragment the free space and the by-block tree grows a level, at which
# point taking a record out can collapse a node and putting one in can
# split one.
#
# HOW MANY RECORDS IT TAKES. A leaf holds `(blocksize - 56) / 8`
# free-space records: 505 at a 4 KiB block, 121 at 1 KiB. So a 1 KiB
# filesystem needs only ~122 separate free runs to force a second level,
# which is a few hundred files rather than a few thousand -- the reason
# these are built at 1 KiB rather than at the default.
#
# HOW THE FRAGMENTS ARE MADE. Fill the group with one-block files and
# then delete every second one. Each deletion returns one block that
# does not adjoin its neighbours, so the count of free-space records
# rises by one per deletion rather than merging back into a single run.
#
#   ./scripts/build-deeptree-fixtures.sh
set -euo pipefail

# WHERE THIS RUNS. Anywhere with xfsprogs and the privilege to
# loop-mount: a CI runner, a container, or the oracle VM.
# `vm-build-deeptree-fixtures.sh` ships this same file into the VM, so
# the two cannot drift.

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

SUDO=""
[ "$(id -u)" -eq 0 ] || SUDO="sudo"

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not found; install xfsprogs" >&2; exit 1; }

mkdir -p "$OUT"

# name:mkfs args:how many files to make before deleting every second one
#
# Two allocation groups, the fewest mkfs.xfs allows -- it refuses one,
# because a filesystem needs a second superblock for redundancy. Files
# are allocated near the directory that names them, so /frag fragments
# group 0 and the fragments do not spread themselves thin across many
# groups, leaving each one's tree short of a split.
FIXTURES=(
    # The free-space trees at two levels, with nothing else unusual.
    "bno2:-m crc=1,rmapbt=0,reflink=0 -b size=1024 -d agcount=2:600"
    # The same, with the reverse-mapping tree along for the ride: it
    # grows with the free space, so it is a second tree at two levels in
    # the same group.
    "rmap2:-m crc=1,rmapbt=1,reflink=0 -b size=1024 -d agcount=2:600"
)

built=0
for spec in "${FIXTURES[@]}"; do
    name="${spec%%:*}"
    rest="${spec#*:}"
    args="${rest%:*}"
    files="${rest##*:}"
    img="$OUT/xfsdeep-$name.img"

    rm -f "$img"
    # mkfs.xfs refuses anything under 300 MB, so the size is its floor
    # rather than a choice: what matters here is the number of separate
    # free runs, not how much room they leave.
    truncate -s "$SIZE" "$img"
    # shellcheck disable=SC2086
    if ! mkfs.xfs -f -q $args "$img" >/dev/null 2>&1; then
        rm -f "$img"
        echo "SKIP  $name (mkfs.xfs rejected $args)"
        continue
    fi

    m=$(mktemp -d)
    $SUDO mount -o loop "$img" "$m"

    $SUDO mkdir -p "$m/frag"
    n=0
    while [ "$n" -lt "$files" ]; do
        # One block each. `dd` rather than `truncate` so the block is
        # really allocated rather than left as a hole.
        $SUDO dd if=/dev/zero of="$m/frag/f$n" bs=1024 count=1 status=none
        n=$((n + 1))
    done
    sync

    # Every second one, so what comes back is a hole between two files
    # rather than a run that merges with its neighbours.
    n=0
    while [ "$n" -lt "$files" ]; do
        $SUDO rm -f "$m/frag/f$n"
        n=$((n + 2))
    done
    sync

    # A directory one entry short of leaving its inode, and a file to
    # write into, so the write oracles have something to do here that
    # allocates.
    $SUDO mkdir -p "$m/sf"
    echo one | $SUDO tee "$m/sf/aaaa" > /dev/null
    echo two | $SUDO tee "$m/sf/bbbb" > /dev/null
    $SUDO touch "$m/sf/empty.bin"
    $SUDO touch "$m/sf/victim"
    sync

    $SUDO umount "$m"; rmdir "$m"

    # WHAT THIS IS FOR, CHECKED RATHER THAN ASSUMED. A fixture built to
    # have a two-level tree and holding a one-level tree looks like a
    # passing test and proves nothing, so the depth is read back off the
    # image and the fixture is thrown away if it is not there.
    levels=$(xfs_db -r -c 'agf 0' -c 'p levels[0]' "$img" 2>/dev/null | awk -F'= ' '{print $2}')
    if [ "${levels:-0}" -lt 2 ]; then
        echo "SKIP  $name (by-block tree came out ${levels:-unknown} level(s), wanted 2+)"
        rm -f "$img"
        continue
    fi
    echo "OK    xfsdeep-$name.img (by-block tree $levels levels)"
    built=$((built + 1))
done

echo "built $built deep-tree fixture(s) in $OUT"
