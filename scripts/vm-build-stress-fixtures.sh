#!/usr/bin/env bash
#
# vm-build-stress-fixtures.sh — run build-stress-fixtures.sh inside the
# oracle VM, for a host that has no xfsprogs and cannot loop-mount.
#
# The rationale for the fixtures themselves, and the licensing note about
# fstests, are in that script. This is only the transport.
#
# The generators must already be built in the guest:
#   (cd tests/vagrant/debian && vagrant provision --provision-with stress-tools)
#
#   ./scripts/vm-build-stress-fixtures.sh
set -euo pipefail

# Bring the machine down when this finishes, however it finishes.
source "$(dirname "${BASH_SOURCE[0]}")/vm-session.sh"

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
guest_path="$("$REPO/scripts/vm.sh" put "$REPO/scripts/build-stress-fixtures.sh")"
"$REPO/scripts/vm.sh" run "XFS_FIXTURE_DIR=/share bash '$guest_path'"
rm -f "$REPO/.vm-share/build-stress-fixtures.sh"
