#!/usr/bin/env bash
# Run a command with an isolated test scratch directory. Raspberry Pi runs use
# the checkout storage so write-heavy fixture work does not churn the SD card.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""

cleanup() {
    if [[ -n "$RUN_DIR" && -d "$RUN_DIR" ]]; then
        find "$RUN_DIR" -depth -mindepth 1 -delete
        rmdir "$RUN_DIR"
    fi
}
trap cleanup EXIT HUP INT TERM

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
"$@"
