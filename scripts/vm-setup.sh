#!/usr/bin/env bash
#
# vm-setup.sh — the fs-linux-test-harness [setup] script. Runs as root
# INSIDE the VM, re-applied by the harness whenever this file changes.
#
# THE GUEST IS WHERE THE ORACLE TOOLS LIVE. Not the host: xfsprogs on a
# workstation is whatever that machine has — nothing at all on a Mac, a
# distribution build on Linux, a different version per developer — and
# the answers it gives are the evidence this suite rests on. One Debian
# guest, one version, the same answers for everyone. The runner is no
# different: CI used to install xfsprogs on the runner and loop-mount
# there with sudo, so the gate and a developer's run were judged by two
# different mkfs and two different kernels.
#
#   xfsprogs    mkfs.xfs, xfs_db, xfs_repair, xfs_logprint, xfs_io —
#               the oracle tools (tests/common/mod.rs) and the fixture
#               builders' formatter
#   attr, acl   setfattr/getfattr, setfacl/getfacl: the extended
#               attributes and ACLs the feature-matrix fixtures carry
#   fdisk       sfdisk, for the partitioned fixtures
#   util-linux  losetup and mount: the kernel oracles' loop mounts, which
#               happen here and nowhere else
#
# AND A PINNED xfsprogs 6.13, because tests/parent_exchrange_oracle.rs
# needs parent pointers and those arrived in 6.10; Debian 12 ships 6.1.
# It goes in its own prefix so every other oracle keeps the
# distribution's mkfs.xfs, whose defaults the fixtures were built with.
#
# AND A RUST TOOLCHAIN, for `chore test:vm` — the whole suite compiled
# and run in here, which is how a macOS host runs a Linux test suite at
# all. It is pinned to the repository's rust-toolchain.toml, installed
# under /var/lib (the VM's own disk, which outlives a `vm:down`), and the
# build directory lives there too so the second run is incremental.
#
# WHAT IS NOT HERE: fsstress and fsx. They are built from fstests, which
# takes minutes, and only `chore fixtures -- stress` wants them —
# scripts/guest-stress-tools.sh installs them on demand from that path.
# A provision every boot pays for is a provision that makes the whole
# suite slower for one set of fixtures nobody builds by default.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive

REPO=/repo
RUST_ROOT=/var/lib/fs-xfs-rust
export RUSTUP_HOME="$RUST_ROOT/rustup"
export CARGO_HOME="$RUST_ROOT/cargo"

# The pinned build's prefix. tests/common/mod.rs names this same path, so
# THE TWO MUST AGREE; it is checked at the end of this script.
PARENT_PREFIX=/usr/local/xfsprogs-parent
PARENT_VERSION=6.13.0

# A GUEST WHOSE PROVISION WAS INTERRUPTED comes back with dpkg half way
# through a transaction, and every later apt-get refuses with "dpkg was
# interrupted, you must manually run 'sudo dpkg --configure -a'". The VM
# outlives a `vm:down`, so that state outlives the run that caused it —
# and the run that caused it is the ordinary one: a reaper stopping a VM
# in the middle of an install, a deadline firing, a laptop closing. It
# costs nothing when there is nothing to finish.
dpkg --configure -a >/dev/null 2>&1 || true

apt-get update -qq
apt-get install -y -qq \
    xfsprogs attr acl fdisk util-linux coreutils \
    curl gcc libc6-dev pkg-config >/dev/null
modprobe xfs

# sed, not head: head exits after one line, mkfs.xfs gets SIGPIPE writing
# its second, and pipefail turns that into a failed setup.
mkfs.xfs -V 2>&1 | sed -n 1p
xfs_db -V 2>&1 | sed -n 1p

# Every tool a test reaches for, proved present here rather than
# discovered missing by the suite. A test never skips on a missing tool,
# so the only useful place to find one absent is the provision.
for tool in mkfs.xfs xfs_db xfs_repair xfs_logprint xfs_io mount losetup sfdisk; do
    command -v "$tool" >/dev/null ||
        { echo "vm-setup: $tool is not installed in the guest" >&2; exit 1; }
done

# THE PINNED xfsprogs, for parent pointers and exchange-range. Built into
# its own prefix on the VM's own disk, so it survives `vm:down` and a
# re-provision is a no-op; `vm:destroy` throws it away with the disk.
if ! "$PARENT_PREFIX/sbin/mkfs.xfs" -V 2>/dev/null | grep -q "version $PARENT_VERSION\$"; then
    echo "vm-setup: building xfsprogs $PARENT_VERSION for parent pointers"
    # g++ AS WELL AS gcc. xfsprogs' configure probes for a C++ compiler
    # and a bare `gcc` package has no cc1plus, so the probe fails with
    # "cannot execute 'cc1plus'" part way through the build rather than
    # at the start, where a missing dependency is easy to read.
    apt-get install -y -qq \
        liburcu-dev libinih-dev uuid-dev libblkid-dev libdevmapper-dev \
        autoconf automake libtool gettext make xz-utils g++ >/dev/null
    work="$(mktemp -d /var/tmp/xfsprogs.XXXXXX)"
    curl -sSfL "https://mirrors.edge.kernel.org/pub/linux/utils/fs/xfs/xfsprogs/xfsprogs-$PARENT_VERSION.tar.xz" |
        tar -xJ -C "$work"
    (
        cd "$work/xfsprogs-$PARENT_VERSION"
        make configure >/dev/null
        # scrub and libicu are for xfs_scrub, which no oracle runs.
        ./configure --prefix="$PARENT_PREFIX" --disable-scrub --disable-libicu >/dev/null
        make -j"$(nproc)" >/dev/null
        make install >/dev/null
    )
    rm -rf "$work"
fi
"$PARENT_PREFIX/sbin/mkfs.xfs" -V

# The toolchain the repository pins, and only that one: a guest that
# silently built with a different compiler than CI is a guest whose
# result means nothing.
toolchain="$(sed -n 's/^channel = "\([^"]*\)"/\1/p' "$REPO/rust-toolchain.toml" | head -1)"
[ -n "$toolchain" ] || { echo "vm-setup: no channel in $REPO/rust-toolchain.toml" >&2; exit 1; }

mkdir -p "$RUST_ROOT"
if [ ! -x "$CARGO_HOME/bin/rustup" ]; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --no-modify-path --default-toolchain none >/dev/null
fi
"$CARGO_HOME/bin/rustup" toolchain install "$toolchain" \
    --component rustfmt --component clippy --profile minimal >/dev/null
"$CARGO_HOME/bin/rustup" default "$toolchain" >/dev/null
"$CARGO_HOME/bin/cargo" --version

# THE PREFIX AND THE TEST MUST AGREE, and nothing else checks that they
# do: tests/common/mod.rs hands the guest this exact path.
grep -q "$PARENT_PREFIX" "$REPO/tests/common/mod.rs" ||
    { echo "vm-setup: tests/common/mod.rs no longer names $PARENT_PREFIX" >&2; exit 1; }

echo "vm-setup: the oracle tools and the pinned toolchain are installed in the guest"
