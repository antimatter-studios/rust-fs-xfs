#!/usr/bin/env bash
#
# build-dirconv-fixtures.sh — a directory before and after the kernel
# converted it from short form to block form.
#
# A short-form directory lives inside its inode. When one more entry will
# not fit, the kernel allocates a filesystem block and writes the whole
# directory into it: `.`, `..`, every existing name and the new one, then
# a hash index, all in the same block. That conversion is what every
# write in this driver refuses, and it is why a directory of about thirty
# short names is the ceiling on everything else.
#
# The pair is the oracle. A conversion cannot be checked by reading the
# directory back: a block could list its entries perfectly and still have
# a hash index in the wrong order, a best-free array that lies about the
# largest gap, or an entry whose tag does not repeat its own offset.
# Comparing against the block the kernel wrote for the same entries is
# what catches those.
#
# # This runs on Linux, and there is only one copy of it
#
# It needs mkfs.xfs and the ability to mount, so it runs either inside
# the harness guest (via scripts/guest-build-fixtures.sh) or directly on a Linux
# CI runner. Both call THIS script.
#
# That is deliberate. The geometry fixtures are built by two scripts —
# one for the VM and one native — and ci.yml carries a comment warning
# that the two must be kept in sync or "CI and a developer's local run
# stop meaning the same thing". They had already drifted once. A second
# pair would be repeating a mistake that is written down.
#
# # The cases
#
#   exact   the entry that overflows is the one that triggers the
#           conversion, and nothing follows it — the smallest case
#   spill   sixteen more entries are added after the conversion, so the
#           block has entries placed into it as well as during its build
#
# There is no case without the file-type feature. mkfs.xfs refuses
# `-n ftype=0` alongside `-m crc=1` — "Directory ftype field always
# enabled on CRC enabled filesystems" — so on v5, the only version this
# driver writes, every entry carries a type byte.
#
# # Why the images are 300 MB
#
# Not a round number chosen for tidiness: it is the floor. Below it
# mkfs.xfs refuses, and it refuses by printing its usage rather than
# saying the device is too small, so a smaller number looks like a
# malformed command line. 200 MB and below were tried and all failed that
# way. The other fixture scripts use 400 MB because they need room for
# the allocator to place files apart; this one cares about a single
# directory block. The files are sparse, so the cost is what mkfs writes
# plus the directory.
#
#   ./scripts/build-dirconv-fixtures.sh
set -euo pipefail

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"
SIZE="${XFS_FIXTURE_SIZE:-300M}"

# Root inside the VM, sudo on a CI runner.
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

mkdir -p "$OUT"

# case:entries-added-after-the-conversion
CASES=(
    "exact:0"
    "spill:16"
)

for entry in "${CASES[@]}"; do
    name="${entry%%:*}"
    after="${entry#*:}"

    before="$OUT/xfsdirconv-$name-before.img"
    final="$OUT/xfsdirconv-$name-after.img"
    rm -f "$before" "$final"

    truncate -s "$SIZE" "$before"
    mkfs.xfs -m crc=1,rmapbt=0 -f -q "$before" >/dev/null

    m="$(mktemp -d)"
    as_root mount -o loop "$before" "$m"
    as_root mkdir "$m/d"

    # Fill the directory to just short of overflowing. How many entries
    # that is depends on the name length and on the inode size, so it is
    # found by asking rather than assumed: add entries until the
    # directory occupies a block of its own, then step back one.
    #
    # A short-form directory occupies no blocks — it lives inside its
    # inode — so the moment `stat` reports any, it has been converted.
    # That is cheaper and more direct than asking xfs_db, which would
    # need the filesystem unmounted.
    n=0
    while [ "$n" -lt 200 ]; do
        as_root touch "$m/d/f$n"
        as_root sync
        if [ "$(as_root stat -c %b "$m/d")" != "0" ]; then break; fi
        n=$((n + 1))
    done
    if [ "$n" -ge 200 ]; then
        echo "FAILED $name — 200 entries did not overflow the inode" >&2
        as_root umount "$m"; rmdir "$m"
        exit 1
    fi
    last="$n"
    as_root rm -f "$m/d/f$last"
    as_root sync
    # Unmounted rather than only synced: a mounted filesystem's metadata
    # is a cache of what is in memory, and the tests read the image.
    as_root umount "$m"

    cp --reflink=never "$before" "$final"
    as_root mount -o loop "$final" "$m"
    # The entry that tips it over.
    as_root touch "$m/d/f$last"
    i=0
    while [ "$i" -lt "$after" ]; do
        as_root touch "$m/d/after$i"
        i=$((i + 1))
    done
    as_root sync
    as_root umount "$m"
    rmdir "$m"

    echo "BUILT  xfsdirconv-$name (converted when entry f$last was added)"
done

echo
echo "Directory-conversion fixtures in $OUT:"
ls -1 "$OUT"/xfsdirconv-*.img 2>/dev/null | sed 's|.*/|  |'
