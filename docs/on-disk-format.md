# On-disk format notes

The parts of the XFS format that are easy to get wrong when reading this crate, and
that have been got wrong here. The format itself is described by the kernel's
[XFS documentation](https://docs.kernel.org/filesystems/xfs/index.html) and the
[XFS filesystem structure](https://www.kernel.org/pub/linux/utils/fs/xfs/docs/xfs_filesystem_structure.pdf)
document; the measured log shapes are in [transaction-shapes.md](transaction-shapes.md),
[buffer-log-item.md](buffer-log-item.md) and [logged-inode.md](logged-inode.md).

## Byte order

**XFS stores every multi-byte on-disk field in big-endian order, on every host.** It is
the only big-endian format in this family — ext4, Btrfs and NTFS are all
little-endian — and it is the first thing to keep in mind when reading this code
alongside its siblings. Every decode goes through `src/endian.rs`, so the rule is
enforced in one place.

With one exception: **checksum fields are stored little-endian**. The kernel's
`xfs_end_cksum()` returns `~cpu_to_le32(crc)`, so a CRC read big-endian like the rest
of the structure makes every real filesystem look corrupt. `endian::le32` exists for
checksum fields and nothing else. This exception has already caused a bug here (see
[below](#what-only-an-oracle-caught)).

The log is a separate case. The published format document says *"all on-disk values
are in big-endian format except the journaling log which is in native endian format"*:
the items a log record carries are in the byte order of the host that wrote them, so
on a little-endian host the logged inode core is a byte-swapped copy of the on-disk
one. [logged-inode.md](logged-inode.md) has the detail.

## Self-describing metadata

On v5, every metadata block carries a CRC32C plus the filesystem UUID, the allocation
group it belongs to, and a log sequence number. The checksum detects corrupted bits;
the identity fields detect a block that is internally *perfect* but landed in the wrong
place — a misdirected write, or a stale block left behind by an earlier filesystem on
the same device. This driver checks both on every header it parses, because a checksum
alone cannot catch the second case.

## The checksum covers the whole sector

The checksum covers the **whole sector**, not the structure — XFS hands the full
buffer length to its verifier, trailing zero padding included. The superblock is a
264-byte structure in a 512-byte (or larger) sector, and its CRC is over all of it.

## What only an oracle caught

Three bugs were live in this crate with the entire unit suite passing:

- the superblock magic had two bytes transposed,
- checksum fields were read big-endian in an otherwise big-endian format that stores
  *checksums* little-endian,
- the checksum was computed over the 264-byte structure instead of the whole sector.

Every one of them is invisible to a round-trip test and fatal against a real
filesystem: the fixtures were built with the same misreading as the parser, and agreed
with it. All three died on the first run against `xfs_db`, which is why every parsed
structure is cross-validated against the reference tools ([testing.md](testing.md)).
