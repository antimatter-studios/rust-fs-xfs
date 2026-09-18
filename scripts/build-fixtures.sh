#!/usr/bin/env bash
#
# build-fixtures.sh [set...]   build the .vm-share fixtures (`chore
#                              fixtures`); name some to build only those,
#                              e.g. `build-fixtures.sh log truncate`
# build-fixtures.sh --check    exit 1 naming every set that is missing
# build-fixtures.sh --sets     print the set names
#
# HOST SIDE, AND ONLY THAT. Every fixture here needs the real kernel's
# XFS driver and the canonical mkfs.xfs to build it, so the work happens
# in the fs-linux-test-harness VM (the sibling checkout at
# ../fs-linux-test-harness, moved to its pinned ref by `chore siblings`):
# scripts/guest-build-fixtures.sh runs as root in the guest against this
# repository, mounted there at /repo, and writes the finished images
# straight into .vm-share.
#
# Nothing is shipped into the guest any more. The ten
# vm-build-<set>-fixtures.sh wrappers existed because the old VM mounted
# .vm-share and nothing else, so a builder had to be copied into the
# share before it could be run; the harness mounts the repository, so the
# builders are simply there.
#
# The VM comes down when this script exits, however it exits
# (FLTH_KEEP_VM=1 keeps it up for a quicker next run).
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
SHARE="$REPO/.vm-share"

# set -> one file that set must have produced. A sentinel rather than the
# whole list: a set either ran or it did not, and the builders' own
# floors (the geometry minimum, the stress verdicts) are what check that
# a set that ran produced enough.
#
# THE ORDER IS THE BUILD ORDER. geometry and data first: several suites
# read those, and the rest are independent of each other.
SENTINELS="geometry:xfs-default.img
data:xfsdata-default.img
log:xfslog-b4096-i512.img
truncate:xfstrunc-lone-before.img
create:xfscreate-spare-before.img
unlink:xfsunlink-spare-before.img
dirconv:xfsdirconv-exact-before.img
crossag:xfscrossag-plain.img
deeptree:xfsdeep-bno2.img
feature-matrix:xfsfeat-base.img
stress:xfsstress-fsx.img"

# `all` is every set but `stress`, which takes tens of minutes and builds
# fsstress and fsx from source first. Saying so out loud because a set
# quietly dropped by a catch-all is the same shape of problem as an
# oracle that skips: `all` that is not all should say which.
ALL="geometry data log truncate create unlink dirconv crossag deeptree feature-matrix"

set_names() { printf '%s\n' "$SENTINELS" | cut -d: -f1; }
sentinel_for() { printf '%s\n' "$SENTINELS" | awk -F: -v s="$1" '$1 == s { print $2 }'; }

case "${1:-}" in
    --sets)
        set_names | tr '\n' ' '
        echo
        exit 0
        ;;
    --check)
        shift
        want="${*:-$ALL}"
        gone=""
        for set in $want; do
            [ -f "$SHARE/$(sentinel_for "$set")" ] || gone="$gone $set"
        done
        if [ -n "$gone" ]; then
            echo "fixtures missing from .vm-share/:$gone" >&2
            echo "build them with 'chore fixtures' — tests never skip on a missing fixture." >&2
            exit 1
        fi
        echo "fixtures: every set present in .vm-share/ ($(echo $want | wc -w) sets)"
        exit 0
        ;;
esac

targets="${*:-$ALL}"
for set in $targets; do
    [ -n "$(sentinel_for "$set")" ] || {
        echo "build-fixtures: unknown set '$set' — one of: $(set_names | tr '\n' ' ')" >&2
        exit 2
    }
done

HARNESS="$REPO/../fs-linux-test-harness"
VM="$HARNESS/scripts/vm.sh"
if [ ! -x "$VM" ]; then
    echo "build-fixtures: the harness is not checked out at $HARNESS." >&2
    echo "                Run 'chore siblings' first." >&2
    exit 1
fi

cd "$REPO"
mkdir -p "$SHARE"
# Brings the VM down when this script ends, however it ends, and fails
# the script if it will not stop.
# shellcheck source=/dev/null
. "$HARNESS/scripts/vm-session.sh"

started=$(date +%s)
# /repo/.vm-share, not /share: both name the same host directory, but
# going through the repository mount means the fixtures, the images a
# test writes and the paths an oracle tool is handed all cross the same
# way. One mount, one set of semantics.
"$VM" run "bash /repo/scripts/guest-build-fixtures.sh /repo/.vm-share $targets"

# CHECKED ON THE HOST rather than taken on the guest's word: every image
# a set claims to have built must carry the XFS superblock magic
# (XFSB, at byte 0 of the filesystem).
for set in $targets; do
    img="$SHARE/$(sentinel_for "$set")"
    [ -f "$img" ] || { echo "build-fixtures: the '$set' set did not produce $(basename "$img")" >&2; exit 1; }
    magic="$(dd if="$img" bs=1 count=4 2>/dev/null)"
    [ "$magic" = XFSB ] || {
        echo "build-fixtures: $(basename "$img") has no XFS superblock magic (got '$magic')" >&2
        exit 1
    }
done

echo "build-fixtures: $(echo $targets | wc -w) set(s) in .vm-share/ ($(( $(date +%s) - started ))s in the VM)"
