#!/usr/bin/env bash
#
# stop-tests.sh — stop a test run and everything it started.
#
# WHY THIS EXISTS. `pkill -f "cargo test"` looks like it stops a run and
# does not. Cargo spawns a separate binary per test target, named
# `target/<profile>/deps/<suite>-<hash>`, and killing cargo leaves those
# running. They keep calling scripts/vm.sh, and vm.sh boots the VM on
# demand -- so every `vagrant halt` is followed by a fresh QEMU seconds
# later and the machine stays loaded with nothing obviously running.
#
# That happened. It took a while to see, because the thing to kill was
# not the thing that had been started.
#
# The binaries now give up on their own when the process that started
# them is gone (see abort_if_orphaned in tests/common/mod.rs). This is
# the direct way to do it, and the way to clean up a run that predates
# that guard.
#
#   ./scripts/stop-tests.sh
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

stopped=0
for pattern in \
    "cargo test" \
    "$REPO/target/release/deps/" \
    "$REPO/target/debug/deps/" \
    "$REPO/scripts/vm.sh"
do
    pids=$(pgrep -f "$pattern" 2>/dev/null | grep -v "^$$\$" || true)
    for pid in $pids; do
        kill -9 "$pid" 2>/dev/null && { echo "stopped $pid ($pattern)"; stopped=$((stopped + 1)); }
    done
done

# Only now: a VM halted while a test binary still lives comes straight
# back, which is the whole reason this script kills in that order.
if [ -x "$REPO/scripts/vm.sh" ]; then
    "$REPO/scripts/vm.sh" down >/dev/null 2>&1 && echo "oracle VM halted"
fi

echo "$stopped process(es) stopped"
