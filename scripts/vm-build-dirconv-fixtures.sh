#!/usr/bin/env bash
#
# vm-build-dirconv-fixtures.sh — run build-dirconv-fixtures.sh inside the
# oracle VM.
#
# The fixtures need mkfs.xfs and the ability to mount, which a macOS host
# has neither of. This copies the builder into the shared folder and runs
# it there; everything about what the fixtures ARE lives in that script
# and not here.
#
# That split is the point. The geometry fixtures have two builders, one
# for the VM and one native, and ci.yml carries a comment warning that
# the two must be kept in sync or "CI and a developer's local run stop
# meaning the same thing" — they had already drifted once. So this is a
# runner and not a copy.
#
#   ./scripts/vm-build-dirconv-fixtures.sh
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Only .vm-share is mounted in the guest, at /share — the repository is
# not. `vm.sh put` copies a file into that folder and echoes its path
# inside the guest.
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-dirconv-fixtures.sh")"

# The shared folder IS the output directory in the guest.
"$REPO/scripts/vm.sh" run "XFS_FIXTURE_DIR=/share bash '$guest_path'"

rm -f "$REPO/.vm-share/build-dirconv-fixtures.sh"
