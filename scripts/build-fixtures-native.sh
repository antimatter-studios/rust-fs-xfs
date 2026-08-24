#!/usr/bin/env bash
#
# build-fixtures-native.sh — build the XFS oracle fixtures directly on a
# Linux host, with no VM.
#
# CI runners are already Linux, so they can run mkfs.xfs and xfs_db
# natively. Developers on macOS get the same fixtures through the VM
# (scripts/vm-build-fixtures.sh); this script is what CI calls.
#
# Both the branch gate (ci.yml) and the release gate (release.yml) call
# THIS script rather than each carrying their own copy of the logic.
# That is the point: the two workflows previously each held their own
# inline copy, they drifted, and the release gate silently stopped
# generating the inode dumps its own test required. A gate that is weaker
# than the one guarding the branch is worse than no gate, because it
# still reports green.
#
# Geometries are chosen to move the fields most likely to be misread:
# block and inode sizes change every log2 field, agcount changes the
# inode-number split, and the feature flags change the AG layout.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SHARE="$REPO/.vm-share"
SIZE="${XFS_FIXTURE_SIZE:-400M}"

# The smallest number of fixtures worth calling a validation. A gate with
# nothing to compare against is not a gate, so a broken mkfs invocation
# must not be able to turn this into a no-op that reports success.
MIN_FIXTURES="${XFS_MIN_FIXTURES:-6}"

# "<name>:<mkfs.xfs args>"
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
)

mkdir -p "$SHARE"

built=0
skipped=0

for geom in "${GEOMETRIES[@]}"; do
    name="${geom%%:*}"
    args="${geom#*:}"
    img="$SHARE/xfs-$name.img"
    sbdump="$SHARE/xfs-$name.sbdump"
    inodedump="$SHARE/xfs-$name.inodedump"

    rm -f "$img" "$sbdump" "$inodedump"
    truncate -s "$SIZE" "$img"

    # shellcheck disable=SC2086 -- $args is a deliberate word split.
    if mkfs.xfs $args -f -q "$img" > "/tmp/mkfs-$name.log" 2>&1; then
        xfs_db -r -c 'sb 0' -c 'print' "$img" > "$sbdump"

        # The root inode dump as well. tests/oracle_vm_fixtures.rs
        # compares the inode parser against these and fails outright when
        # none exist, so that an unvalidated parser cannot pass by having
        # nothing to compare against.
        root="$(xfs_db -r -c 'sb 0' -c 'print rootino' "$img" | awk '{print $3}')"
        xfs_db -r -c "inode $root" -c 'print' "$img" > "$inodedump"

        echo "BUILT  xfs-$name (mkfs.xfs $args, rootino $root)"
        built=$((built + 1))
    else
        # Skipping a geometry this xfsprogs rejects is allowed. Skipping
        # it quietly is not: a fixture that vanishes without a word is a
        # hole in the gate that nobody notices.
        echo "::warning title=XFS geometry skipped::xfs-$name — the installed mkfs.xfs rejected 'mkfs.xfs $args'"
        echo "SKIP   xfs-$name — mkfs.xfs rejected this geometry:"
        sed 's/^/         /' "/tmp/mkfs-$name.log"
        rm -f "$img"
        skipped=$((skipped + 1))
    fi
done

echo
echo "fixtures built: $built, skipped: $skipped"
ls -lh "$SHARE"

if [ "$built" -lt "$MIN_FIXTURES" ]; then
    echo "only $built fixtures built, need at least $MIN_FIXTURES — too few to call this validated" >&2
    exit 1
fi
