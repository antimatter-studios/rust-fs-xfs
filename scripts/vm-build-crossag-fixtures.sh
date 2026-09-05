#!/usr/bin/env bash
#
# vm-build-crossag-fixtures.sh — run build-crossag-fixtures.sh inside the
# oracle VM, for a host that has no xfsprogs and cannot loop-mount.
#
# The rationale for the fixtures is in that script; this is transport.
#
#   ./scripts/vm-build-crossag-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-crossag-fixtures.sh")"
"$REPO/scripts/vm.sh" run "XFS_FIXTURE_DIR=/share bash '$guest_path'"
rm -f "$REPO/.vm-share/build-crossag-fixtures.sh"
