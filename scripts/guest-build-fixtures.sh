#!/usr/bin/env bash
#
# guest-build-fixtures.sh <output-dir> <set>... — GUEST side: runs as
# root inside the fs-linux-test-harness VM (Debian), started by
# scripts/build-fixtures.sh through `vm.sh run`.
#
# THE ONE PLACE FIXTURES ARE BUILT. There used to be twelve: ten
# vm-build-<set>-fixtures.sh wrappers, each of which copied one builder
# into the shared folder and ran it there, plus build-fixtures-native.sh,
# which built the geometry matrix on a CI runner instead, plus the
# geometry loop written inline in vm-build-fixtures.sh. The wrappers were
# byte-identical apart from a filename; the native builder and the inline
# loop were the same work written twice, and they had already drifted
# once (#110). What is left is this dispatcher and the builders
# themselves, which were always the part that said anything.
#
# It also means every fixture is now made by ONE kernel and ONE mkfs.xfs
# — the guest's. A fixture built by the runner's 6.12 kernel and judged
# by the guest's 6.1 is the shape of #211 and #212.
#
# EVERY SET BUILDS ON THE GUEST'S OWN DISK and is copied to <output-dir>
# only when it is finished. Three reasons, and all three have cost time
# here:
#   - a loop mount of a file on the 9p share mixes the guest's page cache
#     with the host's view of the same file, and the host then reads
#     bytes the guest has not written back;
#   - xfs_repair wants the underlying filesystem's geometry and gets
#     ENOTDIR from the share, which reads as a corrupt image;
#   - a builder that fails half way leaves a truncated image where the
#     tests pick fixtures up, and the next run grades the driver against
#     it (#214). Nothing is copied out of a set that failed.
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: guest-build-fixtures.sh <output-dir> <set>..." >&2; exit 2; }
OUTPUT_DIR="$1"
shift

REPO=/repo
SIZE="${XFS_FIXTURE_SIZE:-400M}"

[ -d "$REPO/scripts" ] || {
    echo "guest-build-fixtures.sh: $REPO is not this repository; the harness mounts it there." >&2
    exit 1
}
mkdir -p "$OUTPUT_DIR"

# The geometry list, in the one place that holds it
# (tests/scripts/fixture-geometries-single-copy.sh keeps it that way).
# shellcheck source=scripts/fixture-geometries.sh
source "$REPO/scripts/fixture-geometries.sh"

# The smallest number of geometry fixtures worth calling a validation. A
# gate with nothing to compare against is not a gate, so a broken mkfs
# invocation must not be able to turn this into a no-op that reports
# success.
MIN_GEOMETRY_FIXTURES="${XFS_MIN_FIXTURES:-6}"

# stage <set> — a scratch directory on the guest's own disk for one set.
stage() {
    local dir="/var/tmp/xfs-fixtures-$1"
    rm -rf "$dir"
    mkdir -p "$dir"
    printf '%s\n' "$dir"
}

# publish <set> <dir> — move a finished set into the output directory,
# keeping images sparse. Called only after the builder succeeded.
publish() {
    local set="$1" dir="$2" file base n=0
    shopt -s nullglob
    for file in "$dir"/*; do
        [ -f "$file" ] || continue
        base="$(basename "$file")"
        cp --sparse=always "$file" "$OUTPUT_DIR/$base.partial"
        mv -f "$OUTPUT_DIR/$base.partial" "$OUTPUT_DIR/$base"
        n=$((n + 1))
    done
    shopt -u nullglob
    [ "$n" -gt 0 ] || { echo "guest-build-fixtures: the '$set' set produced no files" >&2; exit 1; }
    rm -rf "$dir"
    echo "[guest] $set: $n file(s)"
}

# THE GEOMETRY MATRIX. Formatted and never mounted, so each log holds one
# unmount record and nothing else — which is the right shape for checking
# the superblock and inode parsers and the wrong one for anything about
# the log (that is the `log` set).
#
# Geometries are chosen to move the fields most likely to be misread:
# block and inode sizes change every log2 field, agcount changes the
# inode-number split, and the feature flags change the AG layout.
build_geometry() {
    local dir built=0 skipped=0 geom name args img root log
    dir="$(stage geometry)"
    for geom in "${XFS_GEOMETRIES[@]}"; do
        name="${geom%%:*}"
        args="${geom#*:}"
        img="$dir/xfs-$name.img"
        log="$dir/.mkfs.log"
        truncate -s "$SIZE" "$img"
        # shellcheck disable=SC2086  # $args is a deliberate word split
        if mkfs.xfs $args -f -q "$img" > "$log" 2>&1; then
            xfs_db -r -c 'sb 0' -c 'print' "$img" > "$dir/xfs-$name.sbdump"
            # The root inode dump as well. tests/oracle_vm_fixtures.rs
            # compares the inode parser against these and fails outright
            # when none exist, so that an unvalidated parser cannot pass
            # by having nothing to compare against.
            root="$(xfs_db -r -c 'sb 0' -c 'print rootino' "$img" | awk '{print $3}')"
            xfs_db -r -c "inode $root" -c 'print' "$img" > "$dir/xfs-$name.inodedump"
            echo "[guest] BUILT xfs-$name (mkfs.xfs $args, rootino $root)"
            built=$((built + 1))
        else
            # A geometry this xfsprogs rejects may be left out. Left out
            # QUIETLY it may not: a fixture that vanishes without a word
            # is a hole in the gate that nobody notices, and the floor
            # below is what stops the hole growing.
            echo "[guest] SKIP  xfs-$name — mkfs.xfs rejected this geometry:"
            sed 's/^/          /' "$log"
            rm -f "$img"
            skipped=$((skipped + 1))
        fi
        rm -f "$log"
    done
    if [ "$built" -lt "$MIN_GEOMETRY_FIXTURES" ]; then
        echo "guest-build-fixtures: only $built geometry fixtures built (skipped $skipped), \
need at least $MIN_GEOMETRY_FIXTURES — too few to call this validated" >&2
        exit 1
    fi
    publish geometry "$dir"
}

# Every other set is a builder of its own, which says what its fixtures
# ARE. This only chooses where they land.
build_with() {
    local set="$1" script="$REPO/scripts/build-$1-fixtures.sh" dir
    [ -f "$script" ] || { echo "guest-build-fixtures: no builder for '$set'" >&2; exit 2; }
    dir="$(stage "$set")"
    # FS_XFS_TEST_TEMP_ACTIVE stops the builders re-executing themselves
    # through scripts/with-test-temp.sh: the scratch directory they would
    # ask for is already here, on the guest's own disk, and re-entering
    # that wrapper inside the guest would put it back on the 9p share.
    XFS_FIXTURE_DIR="$dir" XFS_FIXTURE_SIZE="$SIZE" \
        FS_XFS_TEST_TEMP_ACTIVE=1 TMPDIR=/var/tmp \
        bash "$script"
    publish "$set" "$dir"
}

for set in "$@"; do
    echo "==> $set"
    case "$set" in
        geometry) build_geometry ;;
        stress)
            # fsstress and fsx are built from fstests, which takes
            # minutes, so they are installed from this path rather than
            # by every provision.
            bash "$REPO/scripts/guest-stress-tools.sh"
            build_with stress
            ;;
        *) build_with "$set" ;;
    esac
done

echo "[guest] fixtures in $OUTPUT_DIR:"
ls -1 "$OUTPUT_DIR" | wc -l | xargs echo "[guest]   files:"
