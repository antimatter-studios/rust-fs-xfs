#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
mkdir -p "$REPO/tmp"
SANDBOX="$(mktemp -d "$REPO/tmp/unprivileged-mount-test.XXXXXX")"

cleanup() {
    case "$SANDBOX" in
        "$REPO"/tmp/unprivileged-mount-test.*)
            python3 - "$SANDBOX" <<'PY'
import os
import sys
assert os.path.realpath(sys.argv[1]) == sys.argv[1], 'refusing cleanup through a symlink'
PY
            rm -f "$SANDBOX/sudo" "$SANDBOX/sudo-called" "$SANDBOX/output" \
                "$SANDBOX/touch" "$SANDBOX/hdiutil" "$SANDBOX/bindfs" "$SANDBOX/fusermount3" "$SANDBOX/fusermount" \
                "$SANDBOX/attach.plist" "$SANDBOX/events" "$SANDBOX/detach-refused"
            rmdir "$SANDBOX"
            ;;
        *) echo "FAIL  refusing cleanup outside the mount-test sandbox: $SANDBOX" >&2; return 1 ;;
    esac
}
trap cleanup EXIT
trap 'exit 129' HUP
trap 'exit 130' INT
trap 'exit 143' TERM

cat > "$SANDBOX/sudo" <<'SUDO'
#!/bin/sh
: > "$NO_SUDO_CALLED"
echo 'sudo is unavailable for the unprivileged mount regression' >&2
exit 1
SUDO
chmod +x "$SANDBOX/sudo"

export NO_SUDO_CALLED="$SANDBOX/sudo-called"
export MOUNT_TRACE="$SANDBOX/events" ATTACH_CAPTURE="$SANDBOX/attach.plist"
export DETACH_REFUSED="$SANDBOX/detach-refused"
REAL_TOUCH="$(command -v touch)"
export REAL_TOUCH
cat > "$SANDBOX/touch" <<'TOUCH'
#!/usr/bin/env bash
if [[ "${FAIL_MOUNT_PROBE:-}" == 1 && "$1" == */mounted/must-survive-cleanup ]]; then
    echo 'injected mount-probe write failure' >&2
    exit 86
fi
exec "$REAL_TOUCH" "$@"
TOUCH
chmod +x "$SANDBOX/touch"

case "$(uname -s)" in
    Darwin)
        REAL_HDIUTIL="$(command -v hdiutil)"
        export REAL_HDIUTIL
        cat > "$SANDBOX/hdiutil" <<'HDIUTIL'
#!/usr/bin/env bash
if [[ "$1" == attach ]]; then
    "$REAL_HDIUTIL" "$@" > "$ATTACH_CAPTURE"
    result=$?
    cat "$ATTACH_CAPTURE"
    [[ "$result" == 0 ]] || exit "$result"
    python3 - "$ATTACH_CAPTURE" "$MOUNT_TRACE" <<'PY'
import json
import plistlib
import sys
with open(sys.argv[1], 'rb') as source:
    entity, = plistlib.load(source)['system-entities']
with open(sys.argv[2], 'a') as trace:
    print(json.dumps(dict(op='attach', device=entity['dev-entry'], mount=entity['mount-point'])), file=trace)
PY
elif [[ "$1" == detach ]]; then
    if [[ "${FAIL_DETACH_ONCE:-}" == 1 && ! -e "$DETACH_REFUSED" ]]; then
        : > "$DETACH_REFUSED"
        echo 'injected first-detach failure' >&2
        exit 87
    fi
    "$REAL_HDIUTIL" "$@" || exit "$?"
    python3 - "$MOUNT_TRACE" "$2" <<'PY'
import json
import sys
with open(sys.argv[1], 'a') as trace:
    print(json.dumps(dict(op='detach', device=sys.argv[2])), file=trace)
PY
else
    exec "$REAL_HDIUTIL" "$@"
fi
HDIUTIL
        chmod +x "$SANDBOX/hdiutil"
        ;;
    Linux)
        REAL_MOUNT="$(command -v bindfs)"
        if command -v fusermount3 >/dev/null; then
            REAL_UMOUNT="$(command -v fusermount3)"
            FUSE_UNMOUNT=fusermount3
        else
            REAL_UMOUNT="$(command -v fusermount)"
            FUSE_UNMOUNT=fusermount
        fi
        export REAL_MOUNT REAL_UMOUNT
        cat > "$SANDBOX/bindfs" <<'MOUNT'
#!/usr/bin/env bash
"$REAL_MOUNT" "$@" || exit "$?"
python3 - "$MOUNT_TRACE" "${!#}" <<'PY'
import json
import sys
with open(sys.argv[1], 'a') as trace:
    print(json.dumps(dict(op='attach', device=sys.argv[2], mount=sys.argv[2])), file=trace)
PY
MOUNT
        cat > "$SANDBOX/$FUSE_UNMOUNT" <<'UMOUNT'
#!/usr/bin/env bash
if [[ "${FAIL_DETACH_ONCE:-}" == 1 && ! -e "$DETACH_REFUSED" ]]; then
    : > "$DETACH_REFUSED"
    echo 'injected first-unmount failure' >&2
    exit 87
fi
"$REAL_UMOUNT" "$@" || exit "$?"
python3 - "$MOUNT_TRACE" "${!#}" <<'PY'
import json
import sys
with open(sys.argv[1], 'a') as trace:
    print(json.dumps(dict(op='detach', device=sys.argv[2])), file=trace)
PY
UMOUNT
        chmod +x "$SANDBOX/bindfs" "$SANDBOX/$FUSE_UNMOUNT"
        ;;
esac

assert_balanced_mounts() {
    python3 - "$MOUNT_TRACE" "$1" "$REPO/tmp" <<'PY'
import json
from pathlib import Path
import sys
events = [json.loads(line) for line in Path(sys.argv[1]).read_text().splitlines()]
assert len(events) == 2 * int(sys.argv[2]), events
for attached, detached in zip(events[::2], events[1::2]):
    assert attached['op'] == 'attach' and detached['op'] == 'detach', events
    assert attached['device'] == detached['device'], events
    mount = Path(attached['mount'])
    assert mount.is_relative_to(Path(sys.argv[3])) and mount.name == 'mounted', events
    assert not mount.parent.exists(), f'mount scratch remains after cleanup: {mount.parent}'
PY
}

if ! PATH="$SANDBOX:$PATH" NO_SUDO_CALLED="$SANDBOX/sudo-called" GITHUB_ACTIONS=true \
    bash "$REPO/tests/scripts/test-temp-runner.sh" > "$SANDBOX/output" 2>&1; then
    cat "$SANDBOX/output" >&2
    echo "FAIL  the real mount check must run without passwordless sudo" >&2
    exit 1
fi
if [[ -e "$SANDBOX/sudo-called" ]] || grep -q 'NOT RUN' "$SANDBOX/output"; then
    cat "$SANDBOX/output" >&2
    echo "FAIL  the real mount check requested sudo or did not run" >&2
    exit 1
fi
if ! grep -qx 'PASS  mount boundary and negative control run without sudo' "$SANDBOX/output"; then
    cat "$SANDBOX/output" >&2
    echo "FAIL  the real mount check did not prove its positive and negative controls" >&2
    exit 1
fi

assert_balanced_mounts 2

: > "$MOUNT_TRACE"
set +e
PATH="$SANDBOX:$PATH" GITHUB_ACTIONS=false FAIL_MOUNT_PROBE=1 \
    bash "$REPO/tests/scripts/test-temp-runner.sh" > "$SANDBOX/output" 2>&1
STATUS=$?
set -e
if [[ "$STATUS" == 0 ]] || ! grep -q '^FAIL  the probe could not be written' "$SANDBOX/output"; then
    cat "$SANDBOX/output" >&2
    echo "FAIL  a failed probe write did not fail the mount check explicitly" >&2
    exit 1
fi
assert_balanced_mounts 1

: > "$MOUNT_TRACE"
set +e
PATH="$SANDBOX:$PATH" GITHUB_ACTIONS=false FAIL_DETACH_ONCE=1 \
    bash "$REPO/tests/scripts/test-temp-runner.sh" > "$SANDBOX/output" 2>&1
STATUS=$?
set -e
if [[ "$STATUS" == 0 ]] || ! grep -q '^FAIL  could not .*owned test' "$SANDBOX/output"; then
    cat "$SANDBOX/output" >&2
    echo "FAIL  a failed detach did not fail the mount check explicitly" >&2
    exit 1
fi
assert_balanced_mounts 1

echo "PASS  the mount boundary guard runs with sudo unavailable"
