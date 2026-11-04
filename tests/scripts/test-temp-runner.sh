#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
MOUNT_PLATFORM="$(uname -s)"
FUSE_UNMOUNT=""
if [[ "$MOUNT_PLATFORM" == Linux ]]; then
    if command -v fusermount3 >/dev/null; then
        FUSE_UNMOUNT=fusermount3
    elif command -v fusermount >/dev/null; then
        FUSE_UNMOUNT=fusermount
    else
        echo "FAIL  the rootless mount check needs fusermount3 or fusermount; run 'chore tools'" >&2
        exit 1
    fi
    if ! command -v bindfs >/dev/null; then
        echo "FAIL  the rootless mount check needs bindfs; run 'chore tools'" >&2
        exit 1
    fi
fi
mkdir -p "$REPO/tmp"
TEST_BASE="$(mktemp -d "$REPO/tmp/runner-policy-test.XXXXXX")"
OUTPUT="$TEST_BASE/output.txt"
DESCENDANT_PID_FILE="$TEST_BASE/descendant.pid"
STANDIN="$TEST_BASE/standin"
SLOW_BIN="$TEST_BASE/slow-mktemp"
STARTING_MARK="$TEST_BASE/starting"
MOUNT_LOG="$TEST_BASE/mount.log"
MOUNT_IMAGE="$TEST_BASE/cleanup.dmg"
MOUNT_SOURCE="$TEST_BASE/mount-source"
ATTACH_PLIST="$TEST_BASE/attach.plist"
MOUNT_IN_PROGRESS=0

inside_sandbox() {
    python3 - "$TEST_BASE" "$1" <<'PY'
from pathlib import Path
import sys
root, target = map(Path, sys.argv[1:])
assert root == root.resolve(), f'refusing a symlinked sandbox: {root}'
assert target == target.resolve(), f'refusing a noncanonical cleanup path: {target}'
assert target == root or root in target.parents, f'refusing cleanup outside {root}: {target}'
PY
}

cleanup() {
    case "$TEST_BASE" in
        "$REPO"/tmp/runner-policy-test.*) ;;
        *) echo "FAIL  refusing cleanup outside the runner sandbox: $TEST_BASE" >&2; return 1 ;;
    esac
    inside_sandbox "$TEST_BASE" || return 1
    if [[ "$MOUNT_IN_PROGRESS" == 1 ]]; then
        remove_mount_scratch || return 1
    fi
    inside_sandbox "$MOUNT_SOURCE" || return 1
    rm -f "$OUTPUT" "$DESCENDANT_PID_FILE" "$STARTING_MARK" "$MOUNT_LOG" "$SLOW_BIN/mktemp" \
        "$STANDIN/repo/scripts/with-test-temp.sh" "$STANDIN/fs-linux-test-harness/scripts/vm.sh" \
        "$MOUNT_IMAGE" "$ATTACH_PLIST" "$MOUNT_SOURCE/must-survive-cleanup"
    rmdir "$STANDIN/repo/scripts" "$STANDIN/repo/tmp" "$STANDIN/repo" \
        "$STANDIN/fs-linux-test-harness/scripts" "$STANDIN/fs-linux-test-harness" "$STANDIN" \
        "$SLOW_BIN" "$MOUNT_SOURCE" 2>/dev/null || true
    rmdir "$TEST_BASE" || return 1
}
cleanup_on_exit() {
    local exit_status=$?
    trap - EXIT
    cleanup || exit_status=1
    exit "$exit_status"
}
trap cleanup_on_exit EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM
inside_sandbox "$TEST_BASE"

# STARTUP IS BOUNDED BY A MINUTE, not five seconds: this returns the moment
# the output appears, and on a loaded host a harness session's checks alone
# have taken more than five seconds. The limit costs time only when it fails.
STARTUP_POLLS=1200
wait_for_output() {
    local pid="$1"
    local attempts="$STARTUP_POLLS"
    while [[ ! -s "$OUTPUT" && "$attempts" -gt 0 ]]; do
        if ! kill -0 "$pid" 2>/dev/null; then
            return 1
        fi
        sleep 0.05
        attempts=$((attempts - 1))
    done
    [[ -s "$OUTPUT" ]]
}

process_is_running() {
    local state
    state="$(ps -p "$1" -o stat=)" || return 1
    [[ -n "$state" && "$state" != *Z* ]]
}

if ! python3 - "$(declare -f process_is_running)" <<'PY'
import os
import subprocess
import sys
import time

probe = sys.argv[1] + '\nprocess_is_running "$1"'
child = os.fork()
if child == 0:
    os._exit(0)
try:
    for attempt in range(100):
        state = subprocess.run(['ps', '-p', str(child), '-o', 'stat='],
                               check=True, capture_output=True, text=True).stdout.strip()
        if state.startswith('Z'):
            break
        time.sleep(0.05)
    assert state.startswith('Z'), 'process-state control did not create a zombie'
    os.kill(child, 0)
    for pid, expected in [(os.getpid(), 0), (child, 1)]:
        result = subprocess.run(['bash', '-c', probe, 'process-state-control', str(pid)])
        assert result.returncode == expected, f'process-state control: pid={pid}, expected={expected}, status={result.returncode}'
finally:
    os.waitpid(child, 0)
PY
then
    echo "FAIL  process-state check did not distinguish a live process from an exited zombie" >&2
    exit 1
fi

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

: > "$OUTPUT"
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/with-test-temp.sh" sh -c 'printf "%s\n" "$TMPDIR"; sleep 30' \
    > "$OUTPUT" &
WRAPPER_PID=$!
if ! wait_for_output "$WRAPPER_PID"; then
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  wrapper exited or timed out before reporting its scratch directory" >&2
    exit 1
fi
# Measure signal forwarding after startup, which has its own bounded wait.
# Harness startup time must not consume the two-second TERM deadline.
# A lost signal cannot pass by waiting for the child's sleep to finish.
STARTED="$(date +%s)"
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

# TERM has to reach what the command started, not only the command. Whichever
# wrapper is named, the descendant is a real process with no handler of its
# own, so if it outlives the bounded poll below TERM never reached it: a
# sleeping process with a fatal signal pending is woken at once, and the
# failure is reported rather than waited out.
assert_term_stops_descendant() {
    local wrapper="$1" base="$2" label="$3"
    rm -f "$DESCENDANT_PID_FILE"
    : > "$OUTPUT"
    FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$base" \
        DESCENDANT_PID_FILE="$DESCENDANT_PID_FILE" \
        "$wrapper" sh -c \
            'printf "%s\n" "$TMPDIR"; sleep 30 & echo "$!" > "$DESCENDANT_PID_FILE"; wait' \
        > "$OUTPUT" &
    WRAPPER_PID=$!
    if ! wait_for_output "$WRAPPER_PID"; then
        wait "$WRAPPER_PID" 2>/dev/null || true
        echo "FAIL  $label: descendant test wrapper exited or timed out during startup" >&2
        exit 1
    fi
    for _ in $(seq 1 "$STARTUP_POLLS"); do
        [[ -s "$DESCENDANT_PID_FILE" ]] && break
        sleep 0.05
    done
    if [[ ! -s "$DESCENDANT_PID_FILE" ]]; then
        kill -TERM "$WRAPPER_PID" 2>/dev/null || true
        wait "$WRAPPER_PID" 2>/dev/null || true
        echo "FAIL  $label: descendant test did not report its pid" >&2
        exit 1
    fi
    DESCENDANT_PID="$(cat "$DESCENDANT_PID_FILE")"
    kill -TERM "$WRAPPER_PID"
    set +e
    wait "$WRAPPER_PID"
    STATUS=$?
    set -e
    for _ in $(seq 1 100); do
        ! process_is_running "$DESCENDANT_PID" && break
        sleep 0.05
    done
    SELECTED="$(cat "$OUTPUT")"
    TERM_FAILED=0
    if [[ "$STATUS" -ne 143 ]]; then
        echo "FAIL  $label: process-group TERM returned status $STATUS, expected 143" >&2
        TERM_FAILED=1
    fi
    if [[ -e "$SELECTED" ]]; then
        echo "FAIL  $label: process-group TERM left the scratch directory: $SELECTED" >&2
        TERM_FAILED=1
    fi
    if process_is_running "$DESCENDANT_PID"; then
        echo "FAIL  $label: live descendant survived process-group TERM: pid=$DESCENDANT_PID state=$(ps -p "$DESCENDANT_PID" -o stat= | tr -d " ") pgid=$(ps -p "$DESCENDANT_PID" -o pgid= | tr -d " ")" >&2
        TERM_FAILED=1
    fi
    if [[ "$TERM_FAILED" -ne 0 ]]; then
        kill "$DESCENDANT_PID" 2>/dev/null || true
        exit 1
    fi
    rm -f "$DESCENDANT_PID_FILE"
}

assert_term_stops_descendant "$REPO/scripts/with-test-temp.sh" "$TEST_BASE" "the wrapper as checked out"

# THE SESSION DECIDES THE COMMAND'S PROCESS GROUP, and the wrapper must not
# depend on which it chose. fs-linux-test-harness v0.5.1 ran the command in the
# session's own group; v0.5.4 made it a group of its own and still passes a
# signal to the command alone. Only a host that can run a VM takes this path,
# so a stand-in for each shape holds it on every host and at every pin.
mkdir -p "$STANDIN/repo/scripts" "$STANDIN/repo/tmp" "$STANDIN/fs-linux-test-harness/scripts"
cp "$REPO/scripts/with-test-temp.sh" "$STANDIN/repo/scripts/with-test-temp.sh"
cat > "$STANDIN/fs-linux-test-harness/scripts/vm.sh" <<'SESSION'
#!/usr/bin/env bash
# `vm.sh session <command...>`: the signal goes to the command and no further.
[[ "${1:-}" == session ]] || exit 99
shift
child=""
signalled=0
trap 'signalled=143; [[ -z "$child" ]] || kill -TERM "$child" 2>/dev/null || true' TERM
[[ "$STANDIN_SESSION_SHAPE" != own-group ]] || set -m
"$@" <&0 &
child=$!
set +m
while :; do
    wait "$child"
    status=$?
    kill -0 "$child" 2>/dev/null || break
done
[[ "$signalled" == 0 ]] || status="$signalled"
exit "$status"
SESSION
chmod +x "$STANDIN/fs-linux-test-harness/scripts/vm.sh"
for shape in own-group shared-group; do
    STANDIN_SESSION_SHAPE="$shape" assert_term_stops_descendant \
        "$STANDIN/repo/scripts/with-test-temp.sh" "$STANDIN/repo/tmp" "a session, command in $shape"
done
rm -f "$STANDIN/repo/scripts/with-test-temp.sh" "$STANDIN/fs-linux-test-harness/scripts/vm.sh"
rmdir "$STANDIN/repo/scripts" "$STANDIN/repo/tmp" "$STANDIN/repo" \
    "$STANDIN/fs-linux-test-harness/scripts" "$STANDIN/fs-linux-test-harness" "$STANDIN"

# TERM BEFORE THE PROCESS GROUP EXISTS still stops the run. The scratch
# directory's mktemp is held open for a second so the signal lands while the
# wrapper is starting, ahead of the command it was asked to run; a signal lost
# there leaves the wrapper waiting out the whole command, thirty seconds here.
REAL_MKTEMP="$(command -v mktemp)"
mkdir -p "$SLOW_BIN"
cat > "$SLOW_BIN/mktemp" <<SLOW
#!/bin/sh
case "\$*" in
    *fs-xfs-tests*) : > "$STARTING_MARK"; sleep 1 ;;
esac
exec "$REAL_MKTEMP" "\$@"
SLOW
chmod +x "$SLOW_BIN/mktemp"
: > "$OUTPUT"
rm -f "$STARTING_MARK"
PATH="$SLOW_BIN:$PATH" FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$TEST_BASE" \
    "$REPO/scripts/with-test-temp.sh" sh -c 'printf "%s\n" "$TMPDIR"; sleep 30' \
    > "$OUTPUT" &
WRAPPER_PID=$!
for _ in $(seq 1 "$STARTUP_POLLS"); do
    [[ -e "$STARTING_MARK" ]] && break
    sleep 0.05
done
if [[ ! -e "$STARTING_MARK" ]]; then
    kill -TERM "$WRAPPER_PID" 2>/dev/null || true
    wait "$WRAPPER_PID" 2>/dev/null || true
    echo "FAIL  the wrapper never reached its scratch directory's mktemp" >&2
    exit 1
fi
STARTED="$(date +%s)"
kill -TERM "$WRAPPER_PID"
set +e
wait "$WRAPPER_PID"
STATUS=$?
set -e
ELAPSED=$(( $(date +%s) - STARTED ))
LEFT="$(compgen -G "$TEST_BASE/fs-xfs-tests.*" || true)"
if [[ "$STATUS" -ne 143 || "$ELAPSED" -ge 20 || -n "$LEFT" ]]; then
    echo "FAIL  TERM during startup did not stop the run: status=$STATUS elapsed=$ELAPSED left=$LEFT" >&2
    exit 1
fi
rm -f "$SLOW_BIN/mktemp" "$STARTING_MARK"
rmdir "$SLOW_BIN"

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

# CLEANUP STOPS AT A MOUNT. The command mounts a filesystem below its scratch directory
# and writes a probe into it, and the probe has to outlive the wrapper's cleanup.
# A probe is only on another filesystem if the mount worked, so a mount or a write
# that failed gets a status of its own and is never read as a verdict on cleanup.
case "$MOUNT_PLATFORM" in
    Darwin)
        if ! hdiutil create -size 8m -fs HFS+ -layout NONE -volname xfs-cleanup-test \
            "$MOUNT_IMAGE" </dev/null > "$MOUNT_LOG" 2>&1; then
            echo "FAIL  could not create the owned test image: $(tail -n 3 "$MOUNT_LOG")" >&2
            exit 1
        fi
        MOUNT_COMMAND=(hdiutil attach -plist -nobrowse -noautoopen -noautofsck)
        ;;
    Linux)
        mkdir "$MOUNT_SOURCE"
        inside_sandbox "$MOUNT_SOURCE"
        MOUNT_COMMAND=(bindfs --no-allow-other "$MOUNT_SOURCE")
        ;;
    *) echo "FAIL  no unprivileged mount check is implemented for $MOUNT_PLATFORM" >&2; exit 1 ;;
esac
MOUNT_FAILED=81
PROBE_FAILED=82

run_wrapper_over_mount() {
    local wrapper="$1" base="$2"
    MOUNT_IN_PROGRESS=1
    set +e
    FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$base" \
        MOUNT_FAILED="$MOUNT_FAILED" PROBE_FAILED="$PROBE_FAILED" \
        MOUNT_PLATFORM="$MOUNT_PLATFORM" MOUNT_IMAGE="$MOUNT_IMAGE" ATTACH_PLIST="$ATTACH_PLIST" \
        "$wrapper" sh -c '
            printf "%s\n" "$TMPDIR"
            mkdir "$TMPDIR/mounted" || exit "$MOUNT_FAILED"
            case "$MOUNT_PLATFORM" in
                Darwin) "$@" -mountpoint "$TMPDIR/mounted" "$MOUNT_IMAGE" </dev/null > "$ATTACH_PLIST" || exit "$MOUNT_FAILED" ;;
                Linux) "$@" "$TMPDIR/mounted" || exit "$MOUNT_FAILED" ;;
            esac
            python3 -c "import os,sys; assert os.stat(sys.argv[1]).st_dev != os.stat(sys.argv[2]).st_dev, \"mount did not change filesystem device\"" \
                "$TMPDIR" "$TMPDIR/mounted" || exit "$MOUNT_FAILED"
            touch "$TMPDIR/mounted/must-survive-cleanup" || exit "$PROBE_FAILED"
            exit 23
        ' mount-probe "${MOUNT_COMMAND[@]}" > "$OUTPUT" 2> "$MOUNT_LOG"
    STATUS=$?
    set -e
    SELECTED="$(cat "$OUTPUT")"
    MOUNT_PROBE="$SELECTED/mounted/must-survive-cleanup"
    if [[ "$STATUS" -eq "$MOUNT_FAILED" || "$STATUS" -eq "$PROBE_FAILED" ]]; then
        remove_mount_scratch
        if [[ "$STATUS" -eq "$MOUNT_FAILED" ]]; then
            echo "FAIL  ${MOUNT_COMMAND[*]} did not mount, so cleanup at a mount was not tested: $(tail -n 3 "$MOUNT_LOG")" >&2
        else
            echo "FAIL  the probe could not be written in the mounted filesystem, so cleanup at a mount was not tested: $(tail -n 3 "$MOUNT_LOG")" >&2
        fi
        exit 1
    fi
}

remove_mount_scratch() {
    local selected device
    selected="$(cat "$OUTPUT")"
    if [[ -z "$selected" && ! -s "$ATTACH_PLIST" ]]; then
        MOUNT_IN_PROGRESS=0
        return 0
    fi
    case "$selected" in
        "$TEST_BASE"/fs-xfs-tests.*|"$STANDIN"/repo/tmp/fs-xfs-tests.*) ;;
        *) echo "FAIL  refusing mount cleanup outside the owned scratch base: $selected" >&2; return 1 ;;
    esac
    inside_sandbox "$selected" || return 1
    inside_sandbox "$selected/mounted" || return 1
    if [[ "$MOUNT_PLATFORM" == Darwin && -s "$ATTACH_PLIST" ]]; then
        device="$(python3 - "$ATTACH_PLIST" "$selected/mounted" <<'PY'
import plistlib
import re
import sys
with open(sys.argv[1], 'rb') as source:
    entity, = plistlib.load(source)['system-entities']
assert entity['mount-point'] == sys.argv[2], 'attachment does not name the owned mountpoint'
assert re.fullmatch(r'/dev/disk[0-9]+', entity['dev-entry']), 'unexpected image device'
print(entity['dev-entry'])
PY
)" || return 1
        if ! hdiutil detach "$device" >> "$MOUNT_LOG" 2>&1; then
            echo "FAIL  could not detach the owned test image $device: $(tail -n 3 "$MOUNT_LOG")" >&2
            return 1
        fi
        rm -f "$ATTACH_PLIST"
    elif [[ "$MOUNT_PLATFORM" == Linux && -d "$selected/mounted" ]]; then
        if ! python3 -c 'import os,sys; assert os.stat(sys.argv[1]).st_dev == os.stat(sys.argv[2]).st_dev' \
            "$selected" "$selected/mounted" 2>/dev/null; then
            if ! "$FUSE_UNMOUNT" -u "$selected/mounted" >> "$MOUNT_LOG" 2>&1; then
                echo "FAIL  could not unmount the owned test filesystem: $(tail -n 3 "$MOUNT_LOG")" >&2
                return 1
            fi
        fi
    fi
    if [[ -d "$selected/mounted" ]]; then
        python3 -c 'import os,sys; assert os.stat(sys.argv[1]).st_dev == os.stat(sys.argv[2]).st_dev, "refusing removal across a remaining mount"' \
            "$selected" "$selected/mounted" || return 1
    fi
    if [[ -d "$selected" ]]; then
        find "$selected" -xdev -depth -mindepth 1 -delete || return 1
        rmdir "$selected" || return 1
    fi
    MOUNT_IN_PROGRESS=0
}

run_wrapper_over_mount "$REPO/scripts/with-test-temp.sh" "$TEST_BASE"
if [[ "$STATUS" -ne 23 || ! -e "$MOUNT_PROBE" ]]; then
    remove_mount_scratch
    echo "FAIL  cleanup crossed into a mounted filesystem or changed status: status=$STATUS" >&2
    exit 1
fi
remove_mount_scratch

# The check above can fail. The same wrapper without -xdev has to remove the
# probe, on this system's find and this system's mounted filesystem.
mkdir -p "$STANDIN/repo/scripts" "$STANDIN/repo/tmp"
sed 's/ -xdev / /' "$REPO/scripts/with-test-temp.sh" > "$STANDIN/repo/scripts/with-test-temp.sh"
chmod +x "$STANDIN/repo/scripts/with-test-temp.sh"
if grep -q -e -xdev "$STANDIN/repo/scripts/with-test-temp.sh"; then
    echo "FAIL  the control still has -xdev in its cleanup" >&2
    exit 1
fi
run_wrapper_over_mount "$STANDIN/repo/scripts/with-test-temp.sh" "$STANDIN/repo/tmp"
if [[ "$STATUS" -ne 23 || -e "$MOUNT_PROBE" ]]; then
    remove_mount_scratch
    echo "FAIL  cleanup without -xdev left the probe, so the mount check cannot fail: status=$STATUS" >&2
    exit 1
fi
remove_mount_scratch
rm -f "$STANDIN/repo/scripts/with-test-temp.sh"
rmdir "$STANDIN/repo/scripts" "$STANDIN/repo/tmp" "$STANDIN/repo" "$STANDIN"
echo "PASS  mount boundary and negative control run without sudo"

EXACT="$TEST_BASE/exact"
FS_XFS_TEST_TMPDIR="$EXACT" "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
if [[ "$SELECTED" != "$EXACT" || ! -d "$EXACT" ]]; then
    echo "FAIL  explicit exact scratch directory was not preserved: $SELECTED" >&2
    exit 1
fi
rmdir "$EXACT"

# THE ENVIRONMENT NO LONGER CHOOSES. The wrapper used to fall back to
# $RUNNER_TEMP on a GitHub runner, to the checkout on a Raspberry Pi, and
# to $TMPDIR everywhere else — three different answers for the same
# question, and two of them name a directory the harness guest cannot
# see. The oracle tools run in that guest, which is given this repository
# and nothing else of the host, so scratch is inside the checkout on
# every machine.
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE= \
    GITHUB_ACTIONS=true RUNNER_TEMP="$TEST_BASE" TMPDIR=/must-not-be-used \
    "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
case "$SELECTED" in
    "$REPO"/tmp/fs-xfs-tests.*) ;;
    *)
        echo "FAIL  a runner's own temporary directory was used instead of the checkout: $SELECTED" >&2
        exit 1
        ;;
esac
if [[ -e "$SELECTED" ]]; then
    echo "FAIL  runner did not clean the scratch directory it owned: $SELECTED" >&2
    exit 1
fi

FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE= GITHUB_ACTIONS=false RUNNER_TEMP= \
    env -u TMPDIR "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT"
SELECTED="$(cat "$OUTPUT")"
case "$SELECTED" in
    "$REPO"/tmp/fs-xfs-tests.*) ;;
    *)
        echo "FAIL  the default scratch directory is outside the worktree: $SELECTED" >&2
        exit 1
        ;;
esac
if [[ -e "$SELECTED" ]]; then
    echo "FAIL  runner did not clean its default scratch directory: $SELECTED" >&2
    exit 1
fi

# A base outside the repository is REFUSED, not quietly accepted: an
# image there is a path the guest cannot open, and the failure would
# arrive much later as a tool reporting "No such file or directory"
# about a file that plainly exists.
OUTSIDE="$(mktemp -d)"
set +e
FS_XFS_TEST_TMPDIR= FS_XFS_TEST_TMP_BASE="$OUTSIDE" \
    "$REPO/scripts/test.sh" --print-temp-dir > "$OUTPUT" 2>/dev/null
STATUS=$?
set -e
rmdir "$OUTSIDE"
if [[ "$STATUS" -eq 0 ]]; then
    echo "FAIL  a scratch base outside the repository was accepted: $(cat "$OUTPUT")" >&2
    exit 1
fi

echo "PASS  test runner selects and cleans an owned scratch directory"
