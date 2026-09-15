#!/usr/bin/env bash
# Run a command with an isolated test scratch directory. Raspberry Pi runs use
# the checkout storage so write-heavy fixture work does not churn the SD card.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""
CHILD_PID=""
SIGNAL_STATUS=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        if ! find "$RUN_DIR" -xdev -depth -mindepth 1 -delete; then
            echo "warning: scratch cleanup stopped at a mounted filesystem below $RUN_DIR" >&2
        fi
        if ! rmdir "$RUN_DIR"; then
            echo "warning: scratch directory remains for inspection: $RUN_DIR" >&2
        fi
    fi
}
forward_signal() {
    local signal="$1"
    local number="$2"
    SIGNAL_STATUS=$((128 + number))
    if [[ -n "$CHILD_PID" ]] && kill -0 "$CHILD_PID" 2>/dev/null; then
        kill -s "$signal" "$CHILD_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT
trap 'forward_signal HUP 1' HUP
trap 'forward_signal INT 2' INT
trap 'forward_signal TERM 15' TERM

if [[ -n "${FS_XFS_TEST_TMPDIR:-}" ]]; then
    mkdir -p "$FS_XFS_TEST_TMPDIR"
elif [[ -n "${FS_XFS_TEST_TMP_BASE:-}" ]]; then
    mkdir -p "$FS_XFS_TEST_TMP_BASE"
    RUN_DIR="$(mktemp -d "$FS_XFS_TEST_TMP_BASE/fs-xfs-tests.XXXXXX")"
    export FS_XFS_TEST_TMPDIR="$RUN_DIR"
elif [[ "${GITHUB_ACTIONS:-}" == "true" && -n "${RUNNER_TEMP:-}" ]]; then
    mkdir -p "$RUNNER_TEMP"
    RUN_DIR="$(mktemp -d "$RUNNER_TEMP/fs-xfs-tests.XXXXXX")"
    export FS_XFS_TEST_TMPDIR="$RUN_DIR"
elif [[ -r /proc/device-tree/model ]] && grep -aq 'Raspberry Pi' /proc/device-tree/model; then
    mkdir -p "$REPO/tmp"
    RUN_DIR="$(mktemp -d "$REPO/tmp/fs-xfs-tests.XXXXXX")"
    export FS_XFS_TEST_TMPDIR="$RUN_DIR"
else
    if [[ -n "${TMPDIR:-}" ]]; then
        mkdir -p "$TMPDIR"
        RUN_DIR="$(mktemp -d "$TMPDIR/fs-xfs-tests.XXXXXX")"
    else
        RUN_DIR="$(mktemp -d -t fs-xfs-tests.XXXXXX)"
    fi
    export FS_XFS_TEST_TMPDIR="$RUN_DIR"
fi

export TMPDIR="$FS_XFS_TEST_TMPDIR"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    printf '%s\n' "$FS_XFS_TEST_TMPDIR"
    exit 0
fi

if [[ "$#" -eq 0 ]]; then
    echo "usage: scripts/with-test-temp.sh <command> [args...]" >&2
    exit 2
fi

export FS_XFS_TEST_TEMP_ACTIVE=1
"$@" <&0 &
CHILD_PID=$!
set +e
wait "$CHILD_PID"
STATUS=$?
if [[ -n "$SIGNAL_STATUS" ]]; then
    wait "$CHILD_PID" 2>/dev/null
    STATUS="$SIGNAL_STATUS"
fi
set -e
exit "$STATUS"
