#!/usr/bin/env bash
#
# vm-build-stress-fixtures.sh — build XFS filesystems in the oracle VM
# whose contents were decided by a stress generator rather than by hand.
#
# scripts/vm-build-data-fixtures.sh writes a tree somebody sat down and
# thought of: a small file, a big file, a sparse file, 400 directory
# entries. That is a good tree, and it is also a tree shaped by the same
# assumptions the driver was written under. It will never contain the
# case nobody thought of.
#
# These two fixtures are shaped by `fsstress` and `fsx`, the two stress
# generators from the filesystem test suite (fstests). `fsstress` runs a
# long randomised sequence of filesystem operations — create, link,
# rename, truncate, punch, collapse, insert, mknod, setxattr — against a
# mounted filesystem. `fsx` hammers a single file with randomised reads,
# writes, truncates, hole punches, range clones and mmap operations. The
# on-disk states that come out are far more awkward than anything worth
# writing by hand, and they are produced without reference to what this
# driver happens to find easy.
#
# The suite is NOT run against this driver, and cannot be: it exercises a
# mounted filesystem through the kernel's VFS, and this driver is a
# userspace library. The relationship is the other way round — the
# generator decides what goes on the disk, and the kernel and
# `xfs_repair` remain the only judges of what is on it. That is the same
# arrangement every other fixture here uses; this only widens the input.
#
# LICENSING. fstests is GPL-2.0. Its binaries are executed; nothing from
# it is copied, quoted or adapted into this repository, and its source
# and build tree live only in the guest. Running a program does not make
# the caller a derivative work of it. See
# tests/vagrant/debian/provision-stress-tools.sh, which builds it.
#
# REPRODUCIBILITY. Both generators take a seed and an operation count,
# and both are given fixed ones below with a single worker process. A
# fixture nobody can rebuild is an anecdote, not a test — and a failure
# found against one is not something anyone can bisect. Change the seed
# only deliberately, and expect every manifest to change with it.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
"$REPO/scripts/vm.sh" up

# The fixed inputs. Stated here, recorded in each fixture's .provenance,
# and asserted by tests/stress_oracle.rs.
SEED=20260826
FSSTRESS_OPS=5000
FSX_OPS=20000
FSX_MAXLEN=4194304   # 4 MiB upper bound on the hammered file's length
IMAGE_SIZE=500M      # mkfs.xfs refuses anything under ~300 MB

# Both fixtures are built on the guest's own filesystem rather than in
# /share. The generators issue tens of thousands of small writes with
# fsyncs between them, and driving that through the host share turns a
# one-minute build into a much longer one; xfs_repair cannot open a file
# there at all. The finished image is copied across at the end.
GUEST_WORK=/var/tmp/xfs-stress

# ---------------------------------------------------------------------
# The shared tail: manifest, reference-checker verdict, provenance.
#
# Emitted as one shell function that both cases call, so the two
# fixtures cannot drift into recording different things about
# themselves.
#
# The manifest is generated INSIDE Linux by the kernel's own XFS driver,
# on a read-only mount so the image stays byte-identical to what the
# generator left behind: one line per path, tab separated, as
#
#     <path>\t<type>\t<size>\t<detail>
#
# with the same three types vm-build-data-fixtures.sh emits — dir, link
# (detail is the target), file (detail is the SHA-256) — plus the four a
# hand-written tree never produces and fsstress's `mknod` does: chr,
# blk, fifo and sock. Nothing in this repository decides what the right
# answer is.
# ---------------------------------------------------------------------
read -r -d '' RECORD_FIXTURE <<'GUEST' || true
# record_fixture <case-name> <image> <generator-line>
record_fixture() {
    local name="$1" img="$2" generator="$3"
    local base="$GUEST_WORK/$name"

    # Read-only, and norecovery: the log was left clean by the unmount
    # above, and this must not be the thing that changes the image.
    local mnt; mnt=$(mktemp -d)
    mount -o ro,norecovery,loop "$img" "$mnt"
    (
        cd "$mnt"
        find . -mindepth 1 | sort | while read -r p; do
            rel="${p#.}"
            if   [ -L "$p" ]; then printf '%s\tlink\t0\t%s\n' "$rel" "$(readlink "$p")"
            elif [ -d "$p" ]; then printf '%s\tdir\t0\t-\n'   "$rel"
            elif [ -f "$p" ]; then
                printf '%s\tfile\t%s\t%s\n' "$rel" \
                       "$(stat -c%s "$p")" "$(sha256sum "$p" | cut -d' ' -f1)"
            elif [ -c "$p" ]; then printf '%s\tchr\t0\t-\n'  "$rel"
            elif [ -b "$p" ]; then printf '%s\tblk\t0\t-\n'  "$rel"
            elif [ -p "$p" ]; then printf '%s\tfifo\t0\t-\n' "$rel"
            elif [ -S "$p" ]; then printf '%s\tsock\t0\t-\n' "$rel"
            else                   printf '%s\tunknown\t0\t-\n' "$rel"
            fi
        done
    ) > "$base.manifest"

    # What shape the fixture actually came out. A stress run is random:
    # a seed that happened to produce a flat tree of ten empty files
    # would leave the oracle test green while testing nothing, so the
    # counts are recorded and tests/stress_oracle.rs insists on them.
    #
    # Extent counts come from FIEMAP via the reference tool rather than
    # from this driver, for the same reason as everything else here.
    local extents_max=0 extents_file="-" sparse=0
    while read -r f; do
        local n sz blk
        n=$(filefrag "$mnt$f" 2>/dev/null | sed -n 's/.*: \([0-9]*\) extents\? found.*/\1/p' | head -1)
        [ -n "$n" ] || continue
        if [ "$n" -gt "$extents_max" ]; then extents_max=$n; extents_file=$f; fi
        sz=$(stat -c%s "$mnt$f"); blk=$(stat -c%b "$mnt$f")
        # 512-byte blocks. Fewer allocated than the length implies holes.
        if [ "$sz" -gt $(( blk * 512 )) ]; then sparse=$(( sparse + 1 )); fi
    done < <(awk -F'\t' '$2 == "file" { print $1 }' "$base.manifest")
    umount "$mnt"; rmdir "$mnt"

    {
        printf 'case=%s\n' "$name"
        printf 'generator=%s\n' "$generator"
        printf 'seed=%s\n' "$SEED"
        printf 'fstests=%s\n' "$(cat /usr/local/share/fstests-generators.version 2>/dev/null || echo unknown)"
        printf 'mkfs=%s\n' "$(mkfs.xfs -V 2>&1 | head -1)"
        printf 'kernel=%s\n' "$(uname -r)"
        printf 'entries=%s\n'    "$(wc -l < "$base.manifest")"
        printf 'dirs=%s\n'       "$(awk -F'\t' '$2=="dir"  {n++} END {print n+0}' "$base.manifest")"
        printf 'files=%s\n'      "$(awk -F'\t' '$2=="file" {n++} END {print n+0}' "$base.manifest")"
        printf 'symlinks=%s\n'   "$(awk -F'\t' '$2=="link" {n++} END {print n+0}' "$base.manifest")"
        printf 'chardevs=%s\n'   "$(awk -F'\t' '$2=="chr"  {n++} END {print n+0}' "$base.manifest")"
        printf 'blockdevs=%s\n'  "$(awk -F'\t' '$2=="blk"  {n++} END {print n+0}' "$base.manifest")"
        printf 'fifos=%s\n'      "$(awk -F'\t' '$2=="fifo" {n++} END {print n+0}' "$base.manifest")"
        printf 'sockets=%s\n'    "$(awk -F'\t' '$2=="sock" {n++} END {print n+0}' "$base.manifest")"
        printf 'filebytes=%s\n'  "$(awk -F'\t' '$2=="file" {s+=$3} END {print s+0}' "$base.manifest")"
        printf 'sparsefiles=%s\n' "$sparse"
        printf 'maxextents=%s\n' "$extents_max"
        printf 'maxextentsfile=%s\n' "$extents_file"
    } > "$base.provenance"

    # The reference checker's verdict on the fixture itself. A generator
    # is perfectly capable of tripping over a kernel bug and leaving a
    # genuinely broken filesystem behind; if that ever happens the fixture
    # must be rejected here rather than blamed on the driver. xfs_repair
    # runs on the guest's own filesystem: it wants the host filesystem's
    # geometry and gets ENOTDIR from the share.
    xfs_repair -n "$img" > "$base.repair" 2>&1 && rc=0 || rc=$?
    if [ "$rc" -eq 0 ] && ! grep -qiE 'corrupt|bad magic|would (fix|correct|rebuild|reset)' "$base.repair"; then
        echo SOUND > "$base.verdict"
    else
        echo BROKEN > "$base.verdict"
    fi
}
GUEST

# ---------------------------------------------------------------------
# Case `ops`: a whole tree built by fsstress.
#
# The default operation mix is used unchanged. It is tempting to switch
# off the operations this driver finds least interesting — `mknod` alone
# accounts for a third of the entries — but a mix chosen by whoever is
# writing the fixture is exactly the hand-shaped input this is meant to
# get away from. One worker process, so the order of operations is
# decided by the seed and nothing else.
#
# Built twice, at two block sizes. That is not thoroughness for its own
# sake: a symlink target longer than one block's worth is stored across
# several, and reassembling those is a distinct code path with its own
# way of going wrong. At 4 KiB, fsstress's longest target still fits one
# block and that path never runs. At 1 KiB it takes three.
# ---------------------------------------------------------------------
for variant in "ops:" "ops1k:-b size=1024"; do
  case_name="${variant%%:*}"
  mkfs_args="${variant#*:}"
"$REPO/scripts/vm.sh" run "
    set -e
    CASE=$case_name
    SEED=$SEED
    GUEST_WORK=$GUEST_WORK
    $RECORD_FIXTURE

    command -v fsstress >/dev/null || {
        echo 'fsstress is not installed in the guest. Run:'
        echo '  (cd tests/vagrant/debian && vagrant provision --provision-with stress-tools)'
        exit 1
    }

    mkdir -p \$GUEST_WORK
    img=\$GUEST_WORK/\$CASE.img
    rm -f \$GUEST_WORK/\$CASE.*
    truncate -s $IMAGE_SIZE \$img
    mkfs.xfs -f -q $mkfs_args \$img >/dev/null 2>&1

    mnt=\$(mktemp -d)
    mount -o loop \$img \$mnt
    mkdir -p \$mnt/stress
    # fsstress exits non-zero on an operation the filesystem declined,
    # which is ordinary — the point is the state it leaves behind.
    fsstress -d \$mnt/stress -n $FSSTRESS_OPS -p 1 -s $SEED \
        > \$GUEST_WORK/\$CASE.fsstress.log 2>&1 || true
    sync
    umount \$mnt; rmdir \$mnt

    record_fixture \$CASE \$img 'fsstress -n $FSSTRESS_OPS -p 1 -s $SEED $mkfs_args'

    cp --sparse=always \$img /share/xfsstress-\$CASE.img
    for ext in manifest provenance repair verdict; do
        cp \$GUEST_WORK/\$CASE.\$ext /share/xfsstress-\$CASE.\$ext
    done
    echo \"BUILT xfsstress-\$CASE (\$(wc -l < \$GUEST_WORK/\$CASE.manifest) entries, xfs_repair says \$(cat \$GUEST_WORK/\$CASE.verdict))\"
"
done

# ---------------------------------------------------------------------
# Case `fsx`: one file, hammered.
#
# fsx keeps its own model of what the file should contain and verifies
# after every operation, so a run that completes is a file the kernel
# agrees with byte for byte. What it leaves behind is a small file with
# an absurd extent layout — interleaved written ranges, punched holes,
# collapsed and inserted ranges, and tails left by truncation — which is
# precisely the shape a hand-written fixture never reaches.
# ---------------------------------------------------------------------
"$REPO/scripts/vm.sh" run "
    set -e
    SEED=$SEED
    GUEST_WORK=$GUEST_WORK
    $RECORD_FIXTURE

    command -v fsx >/dev/null || { echo 'fsx is not installed in the guest'; exit 1; }

    mkdir -p \$GUEST_WORK/fsxlogs
    img=\$GUEST_WORK/fsx.img
    rm -f \$GUEST_WORK/fsx.img \$GUEST_WORK/fsx.manifest \$GUEST_WORK/fsx.provenance \
          \$GUEST_WORK/fsx.repair \$GUEST_WORK/fsx.verdict
    truncate -s $IMAGE_SIZE \$img
    mkfs.xfs -f -q \$img >/dev/null 2>&1

    mnt=\$(mktemp -d)
    mount -o loop \$img \$mnt
    mkdir -p \$mnt/fsx
    # Its own logs must not become part of the fixture, so they are kept
    # off the filesystem under test.
    if fsx -N $FSX_OPS -S $SEED -l $FSX_MAXLEN -P \$GUEST_WORK/fsxlogs -q \
           \$mnt/fsx/hammered.bin > \$GUEST_WORK/fsx.fsx.log 2>&1; then
        :
    else
        # fsx failing means fsx and the kernel disagree about the file it
        # just wrote. That is not this driver's problem and the fixture
        # must not be built from it.
        echo 'fsx and the kernel disagreed; refusing to build a fixture from it:'
        tail -20 \$GUEST_WORK/fsx.fsx.log
        umount \$mnt; rmdir \$mnt
        exit 1
    fi
    sync
    umount \$mnt; rmdir \$mnt

    record_fixture fsx \$img 'fsx -N $FSX_OPS -S $SEED -l $FSX_MAXLEN'

    cp --sparse=always \$img /share/xfsstress-fsx.img
    for ext in manifest provenance repair verdict; do
        cp \$GUEST_WORK/fsx.\$ext /share/xfsstress-fsx.\$ext
    done
    echo \"BUILT xfsstress-fsx (\$(sed -n 's/^maxextents=//p' \$GUEST_WORK/fsx.provenance) extents, xfs_repair says \$(cat \$GUEST_WORK/fsx.verdict))\"
"

echo
echo "Stress fixtures in $REPO/.vm-share:"
ls -1 "$REPO/.vm-share"/xfsstress-*.img 2>/dev/null || echo "  (none)"
echo
echo "Now run: cargo test --test stress_oracle -- --nocapture"
