#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
python3 - "$REPO" "$BASH" <<'PY'
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time

repo = Path(sys.argv[1])
bash = sys.argv[2]
(repo / 'tmp').mkdir(exist_ok=True)
sandbox = Path(tempfile.mkdtemp(prefix='temp-descendants.', dir=repo / 'tmp'))
wrapper = None
state = None

def running(pid):
    result = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='],
                            capture_output=True, text=True, timeout=5)
    return result.returncode == 0 and bool(result.stdout.strip()) and 'Z' not in result.stdout

def wait_for(predicate, seconds, message):
    deadline = time.monotonic() + seconds
    while not predicate():
        if time.monotonic() >= deadline:
            raise AssertionError(message)
        time.sleep(0.05)

def stop_owned_processes():
    if wrapper is None:
        return
    groups = {wrapper.pid}
    pids = []
    for name in ('wrapper.pid', 'command.pid', 'job.pid'):
        path = state / name
        if path.exists():
            pid = int(path.read_text())
            pids.append(pid)
            if name != 'job.pid':
                groups.add(pid)
    for group in groups:
        try:
            os.killpg(group, signal.SIGKILL)
        except ProcessLookupError:
            pass
    for pid in pids:
        try:
            os.kill(pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
    wrapper.wait(timeout=5)
    wait_for(lambda: all(not running(pid) for pid in pids), 10,
             'the guard could not stop its own processes')

def interrupted(number, frame):
    raise KeyboardInterrupt(f'signal {number}')

for number in (signal.SIGHUP, signal.SIGINT, signal.SIGTERM):
    signal.signal(number, interrupted)

try:
    job = sandbox / 'job.sh'
    job.write_text('''#!/usr/bin/env bash
set -eu
if [[ "$JOB_KIND" == resistant ]]; then
    trap '' HUP INT TERM
else
    trap '[[ -d "$TMPDIR" ]] || : > "$STATE/removed-while-live"; exit 0' HUP INT TERM
fi
printf '%s\\n' "$$" > "$STATE/job.pid"
: > "$STATE/job.ready"
while :; do
    if [[ ! -d "$TMPDIR" ]]; then
        : > "$STATE/removed-while-live"
    fi
    printf 'alive\\n' > "$TMPDIR/probe" 2>/dev/null || true
    sleep 0.1
done
''')
    command = sandbox / 'command.sh'
    command.write_text('''#!/usr/bin/env bash
set -eu
[[ "$COMMAND_KIND" != resistant ]] || trap '' HUP INT TERM
printf '%s\\n' "$$" > "$STATE/command.pid"
printf '%s\\n' "$TMPDIR" > "$STATE/scratch"
"$BASH" "$JOB" &
while [[ ! -s "$STATE/job.pid" || ! -e "$STATE/job.ready" ]]; do sleep 0.05; done
: > "$STATE/command.ready"
[[ "$COMMAND_KIND" != orphan ]] || exit 23
wait
''')
    session_text = '''#!/usr/bin/env bash
[[ "$1" == session ]] || exit 99
shift
child=''
signalled=0
trap 'signalled=129; [[ -z "$child" ]] || kill -HUP "$child" 2>/dev/null || true' HUP
trap 'signalled=130; [[ -z "$child" ]] || kill -INT "$child" 2>/dev/null || true' INT
trap 'signalled=143; [[ -z "$child" ]] || kill -TERM "$child" 2>/dev/null || true' TERM
[[ "$SESSION_SHAPE" != own-group ]] || set -m
"$@" <&0 &
child=$!
printf '%s\\n' "$child" > "$STATE/wrapper.pid"
set +m
while :; do
    wait "$child"
    status=$?
    kill -0 "$child" 2>/dev/null || break
done
child=''
: > "$STATE/teardown.started"
while [[ ! -e "$STATE/teardown.release" ]]; do sleep 0.05; done
: > "$STATE/teardown.done"
[[ "$signalled" == 0 ]] || status="$signalled"
exit "$status"
'''
    cases = [
        ('no session, TERM-resistant job', 'none', 'waiting', 'resistant', signal.SIGTERM, False),
        ('shared-group session', 'shared-group', 'waiting', 'resistant', signal.SIGTERM, False),
        ('own-group session', 'own-group', 'waiting', 'resistant', signal.SIGTERM, False),
        ('TERM-resistant command', 'own-group', 'resistant', 'resistant', signal.SIGTERM, False),
        ('HUP', 'shared-group', 'waiting', 'resistant', signal.SIGHUP, False),
        ('INT', 'own-group', 'waiting', 'resistant', signal.SIGINT, False),
        ('repeated TERM', 'own-group', 'resistant', 'resistant', signal.SIGTERM, True),
        ('ordinary exit with a surviving job', 'none', 'orphan', 'resistant', None, False),
        ('session exit with a surviving job', 'own-group', 'orphan', 'resistant', None, False),
        ('cooperative job', 'none', 'waiting', 'cooperative', signal.SIGTERM, False),
    ]
    for index, (label, shape, kind, job_kind, stop_signal, repeat) in enumerate(cases):
        state = sandbox / str(index)
        state.mkdir()
        copied_repo = state / 'standin/repo'
        scripts = copied_repo / 'scripts'
        scripts.mkdir(parents=True)
        (copied_repo / 'tmp').mkdir()
        shutil.copy2(repo / 'scripts/with-test-temp.sh', scripts / 'with-test-temp.sh')
        session = state / 'standin/fs-linux-test-harness/scripts/vm.sh'
        if shape != 'none':
            session.parent.mkdir(parents=True)
            session.write_text(session_text)
            session.chmod(0o755)
        env = dict(os.environ, STATE=str(state), JOB=str(job), JOB_KIND=job_kind,
                   COMMAND_KIND=kind, SESSION_SHAPE=shape, FS_XFS_TEST_TMPDIR='',
                   FS_XFS_TEST_TMP_BASE=str(copied_repo / 'tmp'))
        with (state / 'output').open('w') as output:
            wrapper = subprocess.Popen([bash, str(scripts / 'with-test-temp.sh'), bash, str(command)],
                                       env=env, stdin=subprocess.DEVNULL, stdout=output,
                                       stderr=output, start_new_session=True)
            try:
                wait_for(lambda: (state / 'command.ready').exists() or wrapper.poll() is not None,
                         60, f'{label}: the command did not start within 60 seconds')
                assert (state / 'command.ready').exists(), f'{label}: the wrapper exited during startup'
                scratch = Path((state / 'scratch').read_text().strip())
                job_pid = int((state / 'job.pid').read_text())
                if stop_signal is not None:
                    wrapper.send_signal(stop_signal)
                deadline = time.monotonic() + 30
                next_signal = time.monotonic() + 0.2
                while wrapper.poll() is None:
                    assert time.monotonic() < deadline, f'{label}: the wrapper did not end within 30 seconds'
                    if (state / 'teardown.started').exists():
                        assert not running(job_pid), f'{label}: a live descendant reached session teardown'
                        assert not scratch.exists(), f'{label}: session teardown preceded scratch cleanup'
                        if not (state / 'teardown.release').exists():
                            if stop_signal is not None:
                                wrapper.send_signal(stop_signal)
                            (state / 'teardown.release').touch()
                    if repeat and time.monotonic() >= next_signal and not (state / 'teardown.started').exists():
                        wrapper.send_signal(stop_signal)
                        next_signal = time.monotonic() + 0.2
                    time.sleep(0.05)
                expected = 23 if stop_signal is None else 128 + stop_signal
                assert wrapper.returncode == expected, f'{label}: status {wrapper.returncode}, expected {expected}'
                assert not running(job_pid), f'{label}: the wrapper exited and left a TERM-resistant descendant running'
                assert not (state / 'removed-while-live').exists(), f'{label}: scratch was removed while a descendant could write'
                assert not scratch.exists(), f'{label}: owned scratch remains'
                if shape != 'none':
                    assert (state / 'teardown.done').exists(), f'{label}: session teardown was interrupted'
            finally:
                stop_owned_processes()
                wrapper = None
    print(f'PASS  bounded descendant cleanup and session teardown ({len(cases)} cases)')
except (AssertionError, KeyboardInterrupt) as error:
    print(f'FAIL  {error}', file=sys.stderr)
    sys.exit(1)
finally:
    try:
        stop_owned_processes()
    finally:
        assert sandbox.parent == repo / 'tmp' and sandbox.name.startswith('temp-descendants.')
        assert sandbox == sandbox.resolve(), 'refusing a symlinked sandbox'
        shutil.rmtree(sandbox)
PY
