#!/usr/bin/env bash
#
# guest-suite.sh [cargo test args...] — THE WHOLE SUITE, INSIDE THE VM.
#
# The fs-linux-test-harness [test] guest_command: `chore test:vm` (and
# `chore test` on a host that is not Linux) boots the VM and runs this
# from /repo, where the harness mounts this repository.
#
# WHY IT EXISTS. We run the Linux tests on Linux. On a Linux host that is
# the host itself and this is not used. On a Mac there is no XFS, no loop
# mount and no xfsprogs at all — so the suite runs in here, against the
# same sources, with the same pinned toolchain.
#
# The toolchain and the build directory live on the VM's own disk
# (/var/lib, /var/cache), which outlives `vm:down` and is thrown away by
# `vm:destroy`: the first run pays a full build, later runs are
# incremental. The repository itself is a 9p mount, so nothing is written
# back into it except what the tests write to .vm-share and tmp/.
set -euo pipefail

[ "${FLTH_GUEST:-}" = 1 ] ||
    { echo "guest-suite.sh runs INSIDE the harness VM ('chore test:vm')." >&2; exit 1; }

# The path dependencies in Cargo.toml, by directory name.
SIBLINGS="rust-fs-core"

RUST_ROOT=/var/lib/fs-xfs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"
export CARGO_TARGET_DIR=/var/cache/fs-xfs-target
export PATH="$CARGO_HOME/bin:$PATH"

# THE SIBLING CRATES. This crate's Cargo.toml has a path dependency on
# ../rust-fs-core, which on the host is a sibling checkout — and the
# guest is given this repository, not the directory that holds it. The
# harness mounts us at /repo, whose parent IS the guest's root, so
# `../rust-fs-core` resolves to /rust-fs-core: the task stages the
# sibling on the share (from its pinned, clean checkout) and this links
# it into place. `chore siblings` is what keeps it at the right ref.
# shellcheck disable=SC2043  # one sibling today; the list is the point
for sibling in $SIBLINGS; do
    staged="/share/siblings/$sibling"
    [ -d "$staged" ] || {
        echo "guest-suite.sh: $sibling is not staged on the share; 'chore test:vm' does that." >&2
        exit 1
    }
    [ -L "/$sibling" ] || ln -sfn "$staged" "/$sibling"
done

cd /repo
command -v cargo >/dev/null ||
    { echo "guest-suite.sh: no cargo in the guest — 'chore vm:provision' installs it." >&2; exit 1; }

# THROUGH ci-test.sh, like every other way of running this suite: it
# supplies --locked --release itself, refuses a run whose output says it
# skipped, and refuses one that executed fewer tests than its floor. A
# guest run is the path a Mac takes to reach these tests at all, so it is
# the last place that should be graded more loosely than the rest.
#
# WITH THE SELECTION `chore test` USES when the caller names none: every
# suite but the stress corpus, which `chore fixtures` does not build. A
# bare `cargo test` here selected that corpus too, and the Mac path — the
# only way a Mac reaches these tests — failed on three images nothing had
# built.
echo "== in-guest suite: $(uname -srm), $(cargo --version)"
started=$(date +%s)
if [ "$#" -gt 0 ]; then
    scripts/ci-test.sh "$@"
else
    # shellcheck disable=SC2046  # the words are the point
    scripts/ci-test.sh $(scripts/test-targets.sh all)
fi
echo "== in-guest suite: $(( $(date +%s) - started ))s"
