#!/usr/bin/env bash
#
# build-xfsprogs.sh — build a pinned xfsprogs into a prefix, for the oracles
# that need a newer one than the distribution ships.
#
# tests/parent_exchrange_oracle.rs needs parent pointers, which arrived in
# xfsprogs 6.10, and Ubuntu 24.04 has 6.6. Only that suite uses this build:
# it finds it through XFSPROGS_PARENT_BIN, and every other oracle keeps the
# distribution's mkfs.xfs, whose defaults its fixtures were built with.
#
#   ./scripts/build-xfsprogs.sh PREFIX     binaries land in PREFIX/sbin
#
# Needs a C toolchain, autotools, gettext, and the liburcu, libinih, uuid,
# blkid and devmapper headers.
set -euo pipefail

VERSION=6.13.0
PREFIX="${1:?usage: build-xfsprogs.sh PREFIX}"
if "$PREFIX/sbin/mkfs.xfs" -V 2>/dev/null | grep -q "version $VERSION\$"; then
    echo "xfsprogs $VERSION already in $PREFIX"
    exit 0
fi

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
curl -sSfL "https://mirrors.edge.kernel.org/pub/linux/utils/fs/xfs/xfsprogs/xfsprogs-$VERSION.tar.xz" \
    | tar -xJ -C "$work"
cd "$work/xfsprogs-$VERSION"
make configure >/dev/null
# scrub and libicu are for xfs_scrub, which no oracle runs.
./configure --prefix="$PREFIX" --disable-scrub --disable-libicu >/dev/null
make -j"$(nproc)" >/dev/null
make install >/dev/null
"$PREFIX/sbin/mkfs.xfs" -V
