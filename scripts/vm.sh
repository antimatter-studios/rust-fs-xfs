#!/usr/bin/env bash
#
# vm.sh — drive the Debian arm64 oracle VM.
#
#   vm.sh up            boot the VM (idempotent; provisions on first run)
#   vm.sh run <cmd...>  run a command inside the VM
#   vm.sh share         print the host path of the shared directory
#   vm.sh put <file>    copy a file into the shared directory, echo guest path
#   vm.sh down          halt the VM (state is kept; next `up` is fast)
#   vm.sh status        say whether the VM is running (exit 0 = running)
#   vm.sh reap          stop a VM nothing cleaned up (the safety net)
#   vm.sh hold          mark the VM as deliberately left running
#   vm.sh destroy       delete the VM entirely
#
# The VM is the real-Linux oracle: mkfs.xfs, the in-kernel XFS driver and
# xfs_repair are Linux-only, and validating this driver against anything
# less than a real kernel would just be marking our own homework.
#
# The VM is kept running between invocations on purpose. Booting is the
# slow part; an iterate-and-check loop should pay it once.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
VAGRANT_DIR="$REPO/tests/vagrant/debian"
SHARE="$REPO/.vm-share"
# Set when someone asked for the machine directly, so `reap` leaves it
# alone. Inside .vagrant/, which is already gitignored, and beside the
# machine state it describes.
HOLD="$VAGRANT_DIR/.vagrant/keep-running"
# WHO IS USING THE MACHINE RIGHT NOW, which is a different question from
# whether somebody means to keep it (#224).
#
# `reap` runs from `lifecycle: after_all`, so it fires on EVERY chore
# invocation — including one started while another is in the middle of
# using the VM. It used to stop that machine, and the first invocation
# failed with whatever it was doing at the time: a provisioning run left
# `dpkg` half way through a transaction, which then broke the next
# provision too.
#
# A use is recorded two ways, because a use has two shapes:
#
#   - LEASES, one file per live `vm.sh` process that is talking to the
#     guest. It covers a single long call — a fixture build inside one
#     `vm.sh run` — and it is removed by a trap on every exit path. A
#     lease whose process is gone is not a use, and the reaper clears
#     it.
#   - LAST USED, a timestamp. Between two calls of a suite that shells
#     out to `vm.sh run` in a loop there is no process to hold a lease,
#     and that gap is exactly where the reaper fires. Use within
#     `IDLE_SECS` counts as use.
#
# Neither replaces `hold`, which says somebody wants the machine kept
# across invocations. These say somebody is using it in this one.
LEASES="$VAGRANT_DIR/.vagrant/leases"
USED="$VAGRANT_DIR/.vagrant/last-used"
IDLE_SECS="${AM_ORACLE_VM_IDLE:-600}"

# Record that this process is using the machine, until it exits.
take_lease() {
    mkdir -p "$LEASES" 2>/dev/null || return 0
    : > "$LEASES/$$" 2>/dev/null || return 0
    trap 'rm -f "$LEASES/$$"' EXIT INT TERM
    touch_used
}

touch_used() {
    mkdir -p "$(dirname "$USED")" 2>/dev/null || return 0
    date +%s > "$USED" 2>/dev/null || true
}

# Whether a live process holds a lease. Clears the leases of processes
# that no longer exist, which is what a run killed outright leaves.
leased() {
    [ -d "$LEASES" ] || return 1
    local held=1 file pid
    for file in "$LEASES"/*; do
        [ -e "$file" ] || continue
        pid="${file##*/}"
        case "$pid" in
            ''|*[!0-9]*) rm -f "$file"; continue ;;
        esac
        if kill -0 "$pid" 2>/dev/null; then
            held=0
        else
            rm -f "$file"
        fi
    done
    return "$held"
}

# Whether the machine was used recently enough that a gap between two
# calls is the likeliest explanation.
recently_used() {
    [ -f "$USED" ] || return 1
    local when now
    when="$(cat "$USED" 2>/dev/null || echo 0)"
    case "$when" in
        ''|*[!0-9]*) return 1 ;;
    esac
    now="$(date +%s)"
    [ $((now - when)) -lt "$IDLE_SECS" ]
}
# One oracle VM runs at a time, across every repository — see
# scripts/vm-slot.sh for why. Absent (an older checkout, a partial
# copy), everything below still works and the serialisation is simply
# not enforced; that is deliberate, because a missing helper should not
# stop a developer building fixtures.
SLOT="$REPO/scripts/vm-slot.sh"

mkdir -p "$SHARE"

# Check the host has what the VM needs before trying to boot it.
#
# Without this the first symptom of a missing tool is a Vagrant error
# that names a plugin or, worse, a guest that imports and then never
# boots. The repository knows what it requires; it should say so.
require_host_tools() {
    local checker="$REPO/scripts/install-host-tools.sh"
    [ -x "$checker" ] || return 0
    if ! "$checker" --quiet; then
        echo
        echo "The VM cannot start until these are installed." >&2
        exit 1
    fi
}

# Vagrant takes an exclusive lock on a machine for the length of any
# command, including `status`. The test suites run in parallel and each
# one calls in here, so contention is normal rather than exceptional —
# and Vagrant's answer to it is to fail immediately with a message
# telling you to wait, which is what this does on its behalf.
#
# Without this a suite fails with the VM apparently unreachable, skips
# its verification, and reports success. That is the worst of the three
# possible outcomes: the tests that matter most quietly stop running.
LOCK_MESSAGE='locks each machine'
LOCK_TRIES=60
LOCK_WAIT=2

vagrant_locked_out() {
    grep -q "$LOCK_MESSAGE" "$1"
}

vm_up() {
    # `vagrant status` is authoritative but slow-ish; only boot when the
    # machine is not already running.
    require_host_tools
    local tries=0 err running=1
    err=$(mktemp)
    while :; do
        if (cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>"$err") \
                | grep -q ',state,running'; then
            running=0
            break
        fi
        if vagrant_locked_out "$err" && [ "$tries" -lt "$LOCK_TRIES" ]; then
            tries=$((tries + 1))
            sleep "$LOCK_WAIT"
            continue
        fi
        break
    done
    rm -f "$err"

    if [ "$running" -ne 0 ]; then
        # TAKE THE SLOT BEFORE BOOTING, and only when actually booting.
        # A machine that is already up took the slot when it started, so
        # asking again here would deadlock a second `up` against itself.
        if [ -x "$SLOT" ]; then
            "$SLOT" acquire || {
                echo "vm: could not get the oracle slot; not booting a second VM." >&2
                exit 1
            }
        fi
        echo "[vm] booting Debian arm64 oracle (first run provisions, ~2 min)..." >&2
        # Retried, because the forwarded SSH port is not always free the
        # instant the previous machine stops:
        #
        #   qemu-system-aarch64: Could not set up host forwarding rule
        #
        # This became routine once every wrapper started tearing down,
        # which made stop-then-start the normal pattern. Waiting for the
        # port to look free was tried first and does not work — `lsof`
        # reports it free while qemu still cannot bind it — so the retry
        # is here, where the failure actually happens, and covers every
        # cause rather than the one that was guessed at.
        for attempt in 1 2 3; do
            if (cd "$VAGRANT_DIR" && vagrant up); then
                break
            fi
            if [ "$attempt" -eq 3 ]; then
                # Give the slot back. A boot that failed three times is
                # not holding a VM, and leaving the slot taken would make
                # every other repository wait for a machine that will
                # never exist.
                [ -x "$SLOT" ] && "$SLOT" release || true
                echo "vm: the machine would not boot after 3 attempts." >&2
                exit 1
            fi
            echo "[vm] boot failed, retrying in 5s (attempt $attempt of 3)..." >&2
            sleep 5
            # A half-started machine holds the resources the next attempt
            # needs, so it is stopped before trying again.
            (cd "$VAGRANT_DIR" && vagrant halt -f) >/dev/null 2>&1 || true
        done
    fi
}

# Run a script in the guest, waiting out any lock another caller holds.
#
# The script is passed rather than piped in from the caller so a retry
# can send it again: standard input is consumed by the first attempt, and
# a retry that sent nothing would report success having run nothing.
vm_run() {
    local script="$1" tries=0 err
    err=$(mktemp)
    while :; do
        if printf '%s\n' "$script" \
                | (cd "$VAGRANT_DIR" && vagrant ssh -- -T 'sudo bash -s') 2>"$err"; then
            rm -f "$err"
            return 0
        fi
        if vagrant_locked_out "$err" && [ "$tries" -lt "$LOCK_TRIES" ]; then
            tries=$((tries + 1))
            sleep "$LOCK_WAIT"
            continue
        fi
        cat "$err" >&2
        rm -f "$err"
        return 1
    done
}

case "${1:-}" in
    up)
        vm_up
        ;;
    run)
        shift
        take_lease
        vm_up
        # `vagrant ssh -c` mangles quoting for complex commands; feed the
        # command on stdin instead so the guest shell sees it verbatim.
        vm_run "$*"
        ;;
    share)
        echo "$SHARE"
        ;;
    put)
        [ $# -eq 2 ] || { echo "usage: vm.sh put <file>" >&2; exit 2; }
        cp "$2" "$SHARE/"
        echo "/share/$(basename "$2")"
        ;;
    down)
        # Halt, then CONFIRM. `vagrant halt` reporting success is not the
        # same as the machine being down, and this runs as a `defer:` in
        # chores.yml — so its exit status is the only thing standing
        # between a leaked QEMU process and a green test run.
        #
        # A teardown that says it worked while the VM is still up is the
        # exact failure the defer was added to prevent, and it is worse
        # than one that fails loudly: nobody looks again at a green run.
        rm -f "$HOLD"
        (cd "$VAGRANT_DIR" && vagrant halt) || true
        state=$(cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
                | sed -n 's/.*,state,//p' | head -1)
        case "$state" in
            running)
                echo "vm: halt did not stop the machine — it is still running." >&2
                echo "    Left as it is rather than force-killed; \`vm.sh destroy\` reclaims it." >&2
                exit 1
                ;;
            *)
                # poweroff, not_created, aborted, or a status that could
                # not be read because the machine was never made. None of
                # those is a running VM, which is all this promises.
                #
                # The slot is given back HERE rather than beside the
                # `vagrant halt` above, because it is released on the
                # strength of the confirmation, not of the attempt. A
                # halt that reported success while the machine kept
                # running must keep the slot, or the next repository
                # boots a second VM alongside it.
                [ -x "$SLOT" ] && "$SLOT" release || true
                ;;
        esac

        ;;
    status)
        # Answers the question that used to need `ps | grep qemu`: is the
        # machine up. Exits 0 when it is running, 1 when it is not, so it
        # is usable in a condition as well as readable by a person.
        state=$(cd "$VAGRANT_DIR" && vagrant status --machine-readable 2>/dev/null \
                | sed -n 's/.*,state,//p' | head -1)
        case "$state" in
            running)
                echo "vm: running"
                ;;
            *)
                echo "vm: not running (${state:-no machine})"
                exit 1
                ;;
        esac
        ;;
    hold)
        # "I want this machine up; do not reap it."
        #
        # Distinct from KEEP_VM, which says "do not tear down between
        # the steps of THIS run". This one outlives the process that set
        # it, because the thing it records also does: a person who ran
        # `chore vm:up` to work in the guest.
        #
        # A lifecycle hook can see `{{.TASK}}`, the invoked task's name,
        # so it is fair to ask why a file on disk is needed at all. It
        # is because that only describes the CURRENT invocation. The
        # machine has to survive the next one, and the one after that.
        mkdir -p "$(dirname "$HOLD")"
        : > "$HOLD"
        # Piped into `ssh -- -T 'sudo bash -s'` and run from
        # VAGRANT_DIR, matching `run)` above: a bare `vagrant ssh` finds
        # no machine outside that directory, and `ssh -c` mangles
        # quoting. Both mistakes were made here first.
        if printf '%s\n' 'touch /run/am-oracle-vm-held; shutdown -c' \
             | (cd "$VAGRANT_DIR" && vagrant ssh -- -T 'sudo bash -s') \
             >/dev/null 2>&1; then
            echo 'vm: held — reap will leave it up, and the guest deadline is cancelled.' >&2
        else
            echo 'vm: held for reap, but the guest is not reachable: its own' >&2
            echo '    deadline still stands and it will power off on schedule.' >&2
            echo '    Re-run `vm.sh hold` once it is up to cancel that too.' >&2
        fi
        ;;
    reap)
        # The safety net, run from chores.yml as `lifecycle: after_all`
        # so ANY chore invocation cleans up a machine that nothing else
        # did.
        #
        # `defer:` is the primary teardown and stays the primary
        # teardown: it fails the task when it fails, which is what makes
        # a leak impossible to miss. But it only covers a task that
        # reached the step that registered it. It cannot cover a bare
        # `cargo test` — several suites shell out to this script and
        # boot the machine — or a run that was killed outright. This
        # can, on the next chore invocation of any kind.
        #
        # It fails SOFT, and that is deliberate rather than a
        # limitation: chore prints an after_all failure without failing
        # the run, which is right for a net. Turning an unrelated
        # `chore build` red because somebody else left a VM up would
        # teach people to ignore it.
        #
        # The probe is a process check, not `vagrant status`, because
        # this runs on every invocation: ~20ms against ~1s, which is
        # what makes it affordable to have always on.
        if ! pgrep -f "$VAGRANT_DIR/.vagrant/machines" >/dev/null 2>&1; then
            exit 0
        fi
        if [ -f "$HOLD" ]; then
            echo "[vm] left running: it was asked for with \`chore vm:up\`. \`chore vm:down\` stops it." >&2
            exit 0
        fi
        # IN USE BY SOMETHING THAT IS STILL RUNNING (#224). Another chore
        # invocation's after_all must not stop the machine this one is
        # talking to.
        if leased; then
            echo "[vm] left running: in use by another invocation in this checkout." >&2
            exit 0
        fi
        if recently_used; then
            echo "[vm] left running: in use — last call was under $((IDLE_SECS / 60)) minutes ago." >&2
            exit 0
        fi
        echo "vm: a machine was left running by something that did not clean up — stopping it." >&2
        echo "    (a bare \`cargo test\` boots it; \`chore test\` tears it down.)" >&2
        exec "${BASH_SOURCE[0]}" down
        ;;
    destroy)
        rm -f "$HOLD"
        (cd "$VAGRANT_DIR" && vagrant destroy -f)
        # `-f` leaves nothing running whether or not it printed a
        # complaint, so the slot goes back unconditionally. `reap` needs
        # no such line: it execs `down`, which releases on confirmation.
        [ -x "$SLOT" ] && "$SLOT" release || true
        ;;
    *)
        sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
        exit 2
        ;;
esac
