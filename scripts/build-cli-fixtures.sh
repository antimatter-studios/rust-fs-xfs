#!/usr/bin/env bash
#
# build-cli-fixtures.sh — the images the command-line tools are tested on:
# one v5 and one v4 filesystem, each made by mkfs.xfs with a label and
# filled by the Linux kernel with a known tree, and a manifest per image
# of what the kernel says is in it.
#
# WHY A SET OF ITS OWN. This crate has no mkfs, so the `cli` tier cannot
# make the images it reads; the reference tool has to, in the harness
# guest, as every other fixture here is made. The data set comes close
# but has no label (so `get label` would compare null with null) and no
# v4 image, and the v4 image is what the write verbs must refuse.
#
# WHAT EACH IMAGE HOLDS, chosen so every way XFS stores a file is read at
# least once through the tools:
#
#   /small.txt              a one-block extent file
#   /empty                  an empty regular file (no extents at all)
#   /medium.bin             256 KiB in one extent
#   /large.bin              8 MiB
#   /sparse.bin             10 MiB with one written block in the middle
#   /fragmented.bin         200 one-block extents with holes between them:
#                           more than the inode holds, so a B+tree fork
#   /link-short             a symlink stored in the inode
#   /modes/{m0600,m0755,m4755}  permission bits worth reading back
#   /sub/nested/file.txt    a file two directories down
#   /sub/link-remote        a symlink too long for the inode (remote blocks)
#   /manyfiles/entry-N.txt  200 entries: a directory past short form
#
# THE MANIFEST is written by the kernel's own XFS driver on a read-only
# mount, one line per entry, tab-separated:
#
#   <path> <type> <size> <mode> <mtime> <sha256, or the link target, or ->
#
# type is file, dir or symlink; mode is the four octal permission digits
# stat prints; mtime is seconds since the epoch. Nothing in this
# repository decides what the right answer is.
#
# THE .corrupt FILE says where to damage a copy, as offsets the reference
# debugger computed, so the tier and the oracle break the same bytes:
#
#   agi-magic <byte>   the first byte of allocation group 0's AGI header
#   inode-crc <byte>   a byte inside /small.txt's inode core (its mtime),
#                      which on v5 leaves the CRC wrong and on v4 is
#                      not used
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if [[ "${FS_XFS_TEST_TEMP_ACTIVE:-}" != "1" && -x "$SCRIPT_DIR/with-test-temp.sh" ]]; then
    exec "$SCRIPT_DIR/with-test-temp.sh" "$0" "$@"
fi

OUT="${XFS_FIXTURE_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/.vm-share}"

# The same helper every builder that mounts uses: sudo resets PATH, and
# a tool installed for this user would otherwise not be found as root
# (see build-data-fixtures.sh and tests/fixture_builders_keep_their_path.rs).
as_root() {
    if [ "$(id -u)" -eq 0 ]; then
        "$@"
    else
        sudo env PATH="$PATH" HOME="$HOME" "$@"
    fi
}

for tool in mkfs.xfs xfs_db python3 sha256sum; do
    command -v "$tool" >/dev/null || { echo "$tool not found (xfsprogs, python3, coreutils)" >&2; exit 1; }
done

mkdir -p "$OUT"
cd "$OUT"

# "<name>:<label>:<mkfs.xfs args>". 320 MiB is above the smallest
# filesystem current mkfs.xfs will make (300 MiB).
IMAGES=(
    "v5:CLIV5:-m crc=1"
    "v4:CLIV4:-m crc=0"
)

for spec in "${IMAGES[@]}"; do
    name="${spec%%:*}"
    rest="${spec#*:}"
    label="${rest%%:*}"
    args="${rest#*:}"
    img="xfscli-$name.img"

    rm -f "$img" "xfscli-$name.manifest" "xfscli-$name.corrupt"
    truncate -s 320M "$img"
    # shellcheck disable=SC2086  # $args is a deliberate word split
    mkfs.xfs $args -L "$label" -f -q "$img"

    mnt=$(mktemp -d)
    as_root mount -o loop "$img" "$mnt"
    echo 'hello world' | as_root tee "$mnt/small.txt" > /dev/null
    as_root touch "$mnt/empty"
    as_root dd if=/dev/urandom of="$mnt/medium.bin" bs=4096 count=64 status=none
    as_root dd if=/dev/urandom of="$mnt/large.bin" bs=1M count=8 status=none
    as_root truncate -s 10M "$mnt/sparse.bin"
    as_root dd if=/dev/urandom of="$mnt/sparse.bin" bs=4096 count=1 seek=1280 conv=notrunc status=none
    for i in $(seq 0 199); do
        as_root dd if=/dev/urandom of="$mnt/fragmented.bin" bs=4096 count=1 \
            seek=$((i * 2)) conv=notrunc status=none
    done
    as_root ln -s small.txt "$mnt/link-short"
    as_root mkdir -p "$mnt/modes" "$mnt/sub/nested" "$mnt/manyfiles"
    for m in 0600 0755 4755; do
        echo "$m" | as_root tee "$mnt/modes/m$m" > /dev/null
        as_root chmod "$m" "$mnt/modes/m$m"
    done
    echo nested | as_root tee "$mnt/sub/nested/file.txt" > /dev/null
    as_root ln -s "$(python3 -c 'print("r/"*499+"x")')" "$mnt/sub/link-remote"
    for i in $(seq 1 200); do echo "$i" | as_root tee "$mnt/manyfiles/entry-$i.txt" > /dev/null; done
    sync
    as_root umount "$mnt"
    rmdir "$mnt"

    mnt=$(mktemp -d)
    as_root mount -o ro,norecovery,loop "$img" "$mnt"
    ( cd "$mnt"
      as_root find . -mindepth 1 | sort | while read -r p; do
        rel="${p#.}"
        meta="$(stat -c '%a %Y' "$p")"
        mode="$(printf '%04o' "0${meta%% *}")"
        mtime="${meta#* }"
        if [ -L "$p" ]; then
          printf '%s\tsymlink\t%s\t%s\t%s\t%s\n' "$rel" "$(stat -c%s "$p")" "$mode" "$mtime" "$(readlink "$p")"
        elif [ -d "$p" ]; then
          printf '%s\tdir\t0\t%s\t%s\t-\n' "$rel" "$mode" "$mtime"
        else
          printf '%s\tfile\t%s\t%s\t%s\t%s\n' "$rel" "$(stat -c%s "$p")" "$mode" "$mtime" "$(sha256sum "$p" | cut -d' ' -f1)"
        fi
      done
    ) > "xfscli-$name.manifest"
    as_root umount "$mnt"
    rmdir "$mnt"

    # Where to break a copy, from the reference debugger.
    agi="$(xfs_db -r -c 'agi 0' -c 'daddr' "$img" | sed -n 's/^current daddr is //p')"
    ino="$(xfs_db -r -c 'path /small.txt' -c 'inode' "$img" | sed -n 's/^current inode number is //p')"
    # `convert` answers in hex with the decimal in brackets: 0x10200 (66048).
    ino_byte="$(xfs_db -r -c "convert inode $ino byte" "$img" | sed -n 's/.*(\([0-9][0-9]*\)).*/\1/p')"
    if [ -z "$agi" ] || [ -z "$ino" ] || [ -z "$ino_byte" ]; then
        echo "build-cli-fixtures: xfs_db did not locate the AGI (daddr '$agi') or \
/small.txt's inode ('$ino' at byte '$ino_byte')" >&2
        exit 1
    fi
    {
        printf 'agi-magic\t%s\n' "$((agi * 512))"
        # di_mtime is 40 bytes into the core; its nanoseconds half at 44.
        printf 'inode-crc\t%s\n' "$((ino_byte + 44))"
    } > "xfscli-$name.corrupt"

    echo "BUILT xfscli-$name (label $label, $(wc -l < "xfscli-$name.manifest") entries)"
done
