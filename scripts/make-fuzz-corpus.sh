#!/usr/bin/env bash
# Rebuild fuzz/corpus from a real mkfs.xfs image.
#
# The corpus is not a pile of random bytes. Every seed is a structure
# that xfsprogs itself wrote, cut out of one image at a known disk
# address. That is what makes mutation productive: flipping a field in a
# block that is otherwise valid reaches the decoder's interesting paths,
# where random bytes are rejected by the magic number check in the first
# line and never reach anything.
#
# The image is populated through mkfs.xfs's protofile, not by mounting.
# Mounting needs root; a protofile does not, and it still produces
# structures a real xfsprogs wrote -- including the ones an empty
# filesystem does not have. 4000 entries is what pushes the root
# directory past leaf form into node form, and past extents format into
# btree format, so the corpus gets a dir node block and a bmbt block.
#
# It rewrites only the seeds it generates. Anything else under
# fuzz/corpus -- a committed reproducer above all -- is left alone.
#
# Usage: scripts/make-fuzz-corpus.sh [image-size]
set -euo pipefail

size="${1:-512M}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/xfs-fuzz-corpus.XXXXXX")"
trap 'rm -rf "$work"' EXIT

command -v mkfs.xfs >/dev/null || { echo "mkfs.xfs not on PATH" >&2; exit 1; }
command -v xfs_db   >/dev/null || { echo "xfs_db not on PATH" >&2; exit 1; }

: > "$work/empty"
head -c 300000 /dev/urandom > "$work/payload"

{
    echo "fuzz-corpus-seed"
    echo "0 0"
    echo "d--755 0 0"
    for i in $(seq 1 4000); do
        printf 'entry-%04d-with-a-deliberately-longish-name ---644 0 0 %s\n' "$i" "$work/empty"
    done
    echo "big ---644 0 0 $work/payload"
    echo "adir d--755 0 0"
    echo " inner ---644 0 0 $work/payload"
    echo " \$"
    echo "alink l--777 0 0 adir/inner"
    echo "\$"
} > "$work/proto"

img="$work/seed.img"
truncate -s "$size" "$img"
mkfs.xfs -q -f -m crc=1,rmapbt=1,reflink=1 -p "$work/proto" "$img"

# Where each structure landed. The fixed ones are fixed by the format:
# sector 0 is the superblock, then the AGF, AGI and AGFL. The rest are
# read out of the image rather than assumed, because they move with
# geometry.
daddr_of() { xfs_db -r -c "$1" -c "$2" -c daddr "$img" | tail -1 | awk '{print $NF}'; }

bnobt=$(daddr_of "agf 0" "addr bnoroot")
cntbt=$(daddr_of "agf 0" "addr cntroot")
rmapbt=$(daddr_of "agf 0" "addr rmaproot")
refcntbt=$(daddr_of "agf 0" "addr refcntroot")
inobt=$(daddr_of "agi 0" "addr root")

# Block types that have no fixed home are found by their magic. A dir
# leaf and a dir node carry theirs at offset 8, not 0 -- they start with
# the forward and backward sibling pointers of xfs_da3_blkinfo.
eval "$(python3 - "$img" <<'PY'
import struct, sys
blk = 4096
at0 = {b'XDD3': 'dir_data', b'BMA3': 'bmbt'}
at8 = {0x3dff: 'dir_leaf', 0x3ebe: 'dir_node'}
found = {}
with open(sys.argv[1], 'rb') as f:
    off = 0
    while True:
        b = f.read(blk)
        if len(b) < blk:
            break
        name = at0.get(b[:4]) or at8.get(struct.unpack('>H', b[8:10])[0])
        if name and name not in found:
            found[name] = off // 512
        off += blk
for name, sector in found.items():
    print(f"{name}={sector}")
PY
)"

# An inode of each format. The format byte selects an entirely different
# fork layout, so one inode is not a seed for the others.
eval "$(python3 - "$img" <<'PY'
import struct, sys
want = {(0o4, 3): 'inode_dir', (0o10, 2): 'inode_file', (0o12, 1): 'inode_symlink'}
found = {}
with open(sys.argv[1], 'rb') as f:
    off = 0
    while off < 64 * 1024 * 1024 and len(found) < len(want):
        f.seek(off)
        b = f.read(512)
        if len(b) < 512:
            break
        if b[:2] == b'IN':
            mode, = struct.unpack('>H', b[2:4])
            key = (mode >> 12, b[5])
            if key in want and want[key] not in found and mode & 0o777:
                found[want[key]] = off // 512
        off += 512
for name, sector in found.items():
    print(f"{name}={sector}")
PY
)"

cut() {
    local dir="$here/fuzz/corpus/$1" name="$2" sector="$3" sectors="$4"
    [ -n "$sector" ] || { echo "no seed found for $1/$2" >&2; exit 1; }
    mkdir -p "$dir"
    dd if="$img" of="$dir/$name" bs=512 skip="$sector" count="$sectors" status=none
}

# Each seed is overwritten in place; nothing else under fuzz/corpus is
# touched. The corpus is not all generated: the reproducer for every
# defect the fuzzer has found is committed beside these seeds (see
# scripts/fuzz-all.sh), and this used to begin with `rm -rf fuzz/corpus`,
# which threw every one of them away on each rebuild (#258).
# tests/scripts/make-fuzz-corpus-keeps-reproducers.sh holds it to that.
# A seed this script stops generating is therefore left behind, and has
# to be removed by hand along with the line that cut it.
cut superblock     mkfs-crc-rmapbt-reflink.bin  0           1
cut agf            mkfs-ag0.bin                 1           1
cut agi            mkfs-ag0.bin                 2           1
cut agfl           mkfs-ag0.bin                 3           1
cut btree_leaf     bnobt-root.bin               "$bnobt"    8
cut btree_leaf     cntbt-root.bin               "$cntbt"    8
cut btree_leaf     inobt-root.bin               "$inobt"    8
cut btree_leaf     rmapbt-root.bin              "$rmapbt"   8
cut btree_leaf     refcntbt-root.bin            "$refcntbt" 8
cut bmbt           mkfs-bmbt-block.bin          "$bmbt"     8
cut dir_data_block mkfs-data-block.bin          "$dir_data" 8
cut dir_leaf       mkfs-leafn-block.bin         "$dir_leaf" 8
cut dir_node       mkfs-node-block.bin          "$dir_node" 8
cut inode          dir-btree-format.bin         "$inode_dir"     1
cut inode          file-extents-format.bin      "$inode_file"    1
cut inode          symlink-local-format.bin     "$inode_symlink" 1

# Three targets read seeds derived from the ones just cut rather than cut
# themselves: log_dinode (the inodes), extent_list (the bmbt leaf's
# records) and dir_block_form (a block built from the data block's
# entries -- the image has no block-form directory). The derivation is
# code, in tests/fuzz_decoders.rs, which also checks on every run that
# the committed copies still match it.
(cd "$here" && XFS_FUZZ_WRITE_DERIVED_SEEDS=1 \
    cargo test --release --quiet --test fuzz_decoders the_derived_seeds_are_what_their_sources_derive)

echo "corpus rebuilt under fuzz/corpus:"
find "$here/fuzz/corpus" -type f | sort | sed "s#$here/##"
