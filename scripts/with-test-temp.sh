#!/usr/bin/env bash
# Run a command with an isolated test scratch directory.
#
# SCRATCH LIVES IN THE REPOSITORY, always: tmp/ (gitignored), and never
# the system temporary directory or a runner-supplied one. The oracle
# tools run inside the fs-linux-test-harness VM, which sees this
# repository at the path the host knows it by and the shared directory,
# and nothing else of the host — so an image under /tmp or $RUNNER_TEMP
# is a path the tool asked to read it cannot open. It used to pick
# whichever of those the machine offered, which is why the same test read
# a different image depending on where it ran.
#
# FS_XFS_TEST_TMPDIR supplies an exact directory and FS_XFS_TEST_TMP_BASE
# a parent to allocate one under; both must be inside the repository, and
# an exact one is the caller's to delete.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RUN_DIR=""
CHILD_PID=""
CHILD_PGID=""
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
    if [[ -n "$CHILD_PGID" ]] && kill -0 -- "-$CHILD_PGID" 2>/dev/null; then
        kill -s "$signal" -- "-$CHILD_PGID" 2>/dev/null || true
    fi
}
trap cleanup EXIT
trap 'forward_signal HUP 1' HUP
trap 'forward_signal INT 2' INT
trap 'forward_signal TERM 15' TERM

# Inside the repository, or the guest cannot see it. Refused here, where
# the rule can be explained, rather than in the guest as a missing file.
inside_repo() {
    case "$1" in
        "$REPO"/*) return 0 ;;
        *)
            echo "with-test-temp.sh: $2 is $1, which is outside $REPO." >&2
            echo "         The oracle tools run in the harness VM, which sees this" >&2
            echo "         repository and its shared directory and nothing else of the host." >&2
            exit 1
            ;;
    esac
}

if [[ -n "${FS_XFS_TEST_TMPDIR:-}" ]]; then
    inside_repo "$FS_XFS_TEST_TMPDIR" FS_XFS_TEST_TMPDIR
    # An exact caller-supplied directory is not ours to delete.
    mkdir -p "$FS_XFS_TEST_TMPDIR"
else
    base="${FS_XFS_TEST_TMP_BASE:-$REPO/tmp}"
    inside_repo "$base" FS_XFS_TEST_TMP_BASE
    mkdir -p "$base"
    RUN_DIR="$(mktemp -d "$base/fs-xfs-tests.XXXXXX")"
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
# Job control gives the command and every descendant its own process group.
# This is portable to the older Bash shipped by macOS, where `setsid` is not
# available by default.
set -m
"$@" <&0 &
CHILD_PID=$!
CHILD_PGID=$CHILD_PID
set +m
set +e
wait "$CHILD_PID"
STATUS=$?
if [[ -n "$SIGNAL_STATUS" ]]; then
    wait "$CHILD_PID" 2>/dev/null
    STATUS="$SIGNAL_STATUS"
fi
set -e
exit "$STATUS"
