#!/usr/bin/env bash
#
# vm-build-log-fixtures.sh — run build-log-fixtures.sh inside the oracle
# VM, for a host that has no xfsprogs and cannot loop-mount.
#
# The rationale for the fixtures themselves is in that script. This is
# only the transport: one copy of the logic, shipped where it can run.
#
#   ./scripts/vm-build-log-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-log-fixtures.sh")"
"$REPO/scripts/vm.sh" run "XFS_FIXTURE_DIR=/share bash '$guest_path'"
rm -f "$REPO/.vm-share/build-log-fixtures.sh"
