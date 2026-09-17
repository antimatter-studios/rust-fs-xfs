#!/usr/bin/env bash
#
# fixture-geometries.sh — the one list of XFS geometries the oracle
# fixtures are built from.
#
# Sourced by both builders of that matrix:
#   scripts/build-fixtures-native.sh (CI, on the Linux runner)
#   scripts/vm-build-fixtures.sh     (a developer's loop, in the VM)
#
# One copy, because two drifted (#110): the VM list gained `nosparse`,
# which CI never built, and the native list pinned `default` to
# rmapbt=0, which the VM left to whichever mkfs.xfs the guest carried.
# CI and a developer's local run stop meaning the same thing the moment
# the lists differ.
#
# Geometries are chosen to move the fields most likely to be misread:
# block and inode sizes change every log2 field, agcount changes the
# inode-number split, and the feature flags change the AG layout.
#
# Each entry is "<name>:<mkfs.xfs args>".

# `default` is pinned to rmapbt=0 rather than left to mkfs. It is the
# image log_replay_oracle WRITES to, and this driver refuses a read-write
# mount of a filesystem with the reverse-mapping tree because it does not
# maintain one. mkfs.xfs 6.6 turns rmapbt on by default and older ones do
# not, so leaving it unpinned makes the write oracle's fixture depend on
# which xfsprogs the host happens to have. The `reflink` case keeps an
# rmapbt=1 image, which is what the refusal itself is tested against.
#
# shellcheck disable=SC2034  # consumed by the scripts that source this
XFS_GEOMETRIES=(
    "default:-m rmapbt=0"
    "1k:-b size=1024"
    "2k:-b size=2048"
    "i512:-i size=512"
    "i1k:-i size=1024"
    "4ags:-d agcount=4"
    "8ags:-d agcount=8"
    "reflink:-m reflink=1,rmapbt=1"
    "bigtime:-m bigtime=1"
    "nocrc:-m crc=0"
    # A v5 filesystem with sparse inodes turned off. The inode B+tree
    # record packs a hole mask and a chunk count into the four bytes a v4
    # record spends on a single free count, and without an image that has
    # v5 metadata and no sparse inodes there is no way to tell whether
    # that packing follows the format version or the feature
    # (tests/inode_btree_oracle.rs requires one).
    "nosparse:-m crc=1 -i sparse=0"
)
