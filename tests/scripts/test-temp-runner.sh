#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
TEST_BASE="$REPO/tmp/runner-policy-test"
OUTPUT="$REPO/tmp/runner-policy-output.txt"

cleanup() {
    rm -f "$OUTPUT"
    rmdir "$TEST_BASE" 2>/dev/null || true
}
trap cleanup EXIT HUP INT TERM

wait_for_output() {
    local pid="$1"
    local attempts=100
    while [[ ! -s "$OUTPUT" && "$attempts" -gt 0 ]]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 1
        fi
        sleep 0.05
        attempts=$((attempts - 1))
    done
    [[ -s "$OUTPUT" ]]
}

mkdir -p "$TEST_BASE"
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"

case "$SELECTED" in
    "$TEST_BASE"/fs-xfs-tests.*) ;;
    *)
        echo "FAIL  selected scratch directory is outside the requested base: $SELECTED" >&2
        exit 1
        ;;
esac

if [[ -e "$SELECTED" ]]; then
    echo "FAIL  runner did not clean its owned scratch directory: $SELECTED" >&2
    exit 1
fi

FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/with-test-temp.sh" sh -c 'printf "%s\n" "$TMPDIR"; touch "$TMPDIR/probe"' \
    > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
if [[ -e "$SELECTED" ]]; then
    echo "FAIL  command wrapper did not clean the scratch directory it populated: $SELECTED" >&2
    exit 1
fi

set +e
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/with-test-temp.sh" sh -c 'printf "%s\n" "$TMPDIR"; touch "$TMPDIR/probe"; exit 23' \
    > "$OUTPUT"
STATUS=$?
set -e
SELECTED="$(cat "$OUTPUT")"
if [[ "$STATUS" -ne 23 || -e "$SELECTED" ]]; then
    echo "FAIL  command failure status or scratch cleanup was lost: status=$STATUS path=$SELECTED" >&2
    exit 1
fi

STARTED="$(date +%s)"
: > "$OUTPUT"
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/with-test-temp.sh" sh -c 'printf "%s\n" "$TMPDIR"; sleep 2' \
    > "$OUTPUT" &
WRAPPER_PID=$!
if ! wait_for_output "$WRAPPER_PID"; then
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  wrapper exited or timed out before reporting its scratch directory" >&2
    exit 1
fi
kill -TERM "$WRAPPER_PID"
set +e
wait "$WRAPPER_PID"
STATUS=$?
set -e
ELAPSED=$(( $(date +%s) - STARTED ))
SELECTED="$(cat "$OUTPUT")"
if [[ "$STATUS" -ne 143 || "$ELAPSED" -ge 2 || -e "$SELECTED" ]]; then
    echo "FAIL  TERM was not forwarded promptly with status 143 and cleanup: status=$STATUS elapsed=$ELAPSED path=$SELECTED" >&2
    exit 1
fi

: > "$OUTPUT"
DESCENDANT_PID_FILE="$TEST_BASE/descendant.pid"
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    DESCENDANT_PID_FILE="$DESCENDANT_PID_FILE" \
    "$REPO/scripts/with-test-temp.sh" sh -c \
        'printf "%s\n" "$TMPDIR"; sleep 30 & echo "$!" > "$DESCENDANT_PID_FILE"; wait' \
    > "$OUTPUT" &
WRAPPER_PID=$!
if ! wait_for_output "$WRAPPER_PID"; then
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  descendant test wrapper exited or timed out during startup" >&2
    exit 1
fi
for _ in $(seq 1 100); do
    [[ -s "$DESCENDANT_PID_FILE" ]] && break
    sleep 0.05
done
if [[ ! -s "$DESCENDANT_PID_FILE" ]]; then
    kill -TERM "$WRAPPER_PID" 2>/dev/null || true
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  descendant test did not report its pid" >&2
    exit 1
fi
DESCENDANT_PID="$(cat "$DESCENDANT_PID_FILE")"
kill -TERM "$WRAPPER_PID"
set +e
wait "$WRAPPER_PID"
STATUS=$?
set -e
for _ in $(seq 1 100); do
    ! kill -0 "$DESCENDANT_PID" 2>/dev/null && break
    sleep 0.05
done
if [[ "$STATUS" -ne 143 || -e "$(cat "$OUTPUT")" ]] || kill -0 "$DESCENDANT_PID" 2>/dev/null; then
    kill "$DESCENDANT_PID" 2>/dev/null || true
    echo "FAIL  TERM did not stop the command's process group before cleanup: status=$STATUS" >&2
    exit 1
fi
rm -f "$DESCENDANT_PID_FILE"

STARTUP_BLOCKER="$TEST_BASE/not-a-directory"
: > "$STARTUP_BLOCKER"
: > "$OUTPUT"
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$STARTUP_BLOCKER/child" \
    "$REPO/scripts/with-test-temp.sh" true > "$OUTPUT" 2>/dev/null &
WRAPPER_PID=$!
if wait_for_output "$WRAPPER_PID"; then
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  invalid scratch base unexpectedly reached the child command" >&2
    exit 1
fi
set +e
wait "$WRAPPER_PID"
STATUS=$?
set -e
if [[ "$STATUS" -eq 0 ]]; then
    echo "FAIL  invalid scratch base returned success" >&2
    exit 1
fi
rm -f "$STARTUP_BLOCKER"

if sudo -n true 2>/dev/null; then
    set +e
    FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
        "$REPO/scripts/with-test-temp.sh" sh -c '
            printf "%s\n" "$TMPDIR"
            mkdir "$TMPDIR/mounted"
            sudo mount -t tmpfs -o size=1m tmpfs "$TMPDIR/mounted"
            touch "$TMPDIR/mounted/must-survive-cleanup"
            exit 23
        ' > "$OUTPUT" 2>/dev/null
    STATUS=$?
    set -e
    SELECTED="$(cat "$OUTPUT")"
    MOUNT_PROBE="$SELECTED/mounted/must-survive-cleanup"
    if [[ "$STATUS" -ne 23 || ! -e "$MOUNT_PROBE" ]]; then
        sudo umount "$SELECTED/mounted" 2>/dev/null || true
        find "$SELECTED" -xdev -depth -mindepth 1 -delete 2>/dev/null || true
        rmdir "$SELECTED" 2>/dev/null || true
        echo "FAIL  cleanup crossed into a mounted filesystem or changed status: status=$STATUS" >&2
        exit 1
    fi
    sudo umount "$SELECTED/mounted"
    rmdir "$SELECTED/mounted" "$SELECTED"
fi

EXACT="$TEST_BASE/exact"
FS_XFS_TEST_TMPDIR="$EXACT" "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
if [[ "$SELECTED" != "$EXACT" || ! -d "$EXACT" ]]; then
    echo "FAIL  explicit exact scratch directory was not preserved: $SELECTED" >&2
    exit 1
fi
rmdir "$EXACT"

FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE= \
    GITHUB_ACTIONS=true RUNNER_TEMP="$TEST_BASE" TMPDIR=/must-not-be-used \
    "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"

case "$SELECTED" in
    "$TEST_BASE"/fs-xfs-tests.*) ;;
    *)
        echo "FAIL  GitHub scratch directory is outside RUNNER_TEMP: $SELECTED" >&2
        exit 1
        ;;
esac

if [[ -e "$SELECTED" ]]; then
    echo "FAIL  runner did not clean its GitHub scratch directory: $SELECTED" >&2
    exit 1
fi

FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE= GITHUB_ACTIONS=false RUNNER_TEMP= \
    env -u TMPDIR "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
if [[ -r /proc/device-tree/model ]] && grep -aq 'Raspberry Pi' /proc/device-tree/model; then
    case "$SELECTED" in
        "$REPO"/tmp/fs-xfs-tests.*) ;;
        *)
            echo "FAIL  Raspberry Pi scratch directory is outside the worktree: $SELECTED" >&2
            exit 1
            ;;
    esac
else
    case "$(basename "$SELECTED")" in
        fs-xfs-tests.*) ;;
        *)
            echo "FAIL  platform fallback did not create an isolated scratch directory: $SELECTED" >&2
            exit 1
            ;;
    esac
fi
if [[ -e "$SELECTED" ]]; then
    echo "FAIL  runner did not clean its default scratch directory: $SELECTED" >&2
    exit 1
fi

echo "PASS  test runner selects and cleans an owned scratch directory"
