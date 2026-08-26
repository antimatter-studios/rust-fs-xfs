# The logged inode core

When XFS logs an inode it writes a copy of the core into the record. That copy is not the
on-disk inode: it is byte-swapped, its checksum is blank, and on older filesystems it is
a different size.

Established by differential analysis against filesystems the kernel wrote — 24 controlled
A/B pairs with a fixed UUID and one variable changed, plus a byte census over 527 logged
cores, plus a direct comparison against the on-disk inode.

## The shape, in one sentence

**The 176-byte log core is the v3 on-disk dinode, field-for-field at identical offsets**,
differing only in endianness and in `di_crc` being zeroed. Confirmed byte-for-byte by
dumping an on-disk inode alongside its log copy.

So an encoder is a byte-swap of a structure this driver already parses, not a second
layout to maintain.

## Fields

| off | w | field | how it was pinned |
|---|---|---|---|
| 0 | 2 | `di_magic` 0x494E | constant across 527 cores; disk copy is the other byte order |
| 2 | 2 | `di_mode` | chmod 0644 vs 0600 |
| 4 | 1 | `di_version` | 3 on v5; **2 on v4, where the core is 96 bytes** |
| 5 | 1 | `di_format` | 1-entry vs 400-entry directory; short vs long symlink |
| 6 | 2 | unused | constant 0, v4 included |
| 8 | 4 | `di_uid` | chown 4321 vs 8765 |
| 12 | 4 | `di_gid` | chown :4321 vs :8765 |
| 16 | 4 | `di_nlink` | 1 vs 2 hard links |
| 20 | 2 | `di_projid_lo` | chproj 100000 vs 200000 |
| 22 | 2 | `di_projid_hi` | the high halves of the same pair |
| 24 | 8 | pad — **or `di_big_nextents` under `nrext64`** | 0 in 527 cores; holds the data-extent count when the feature is on |
| 32 | 8 | `di_atime` | two controlled `touch -a` values, exact to the nanosecond |
| 40 | 8 | `di_mtime` | likewise, including differing sub-second parts |
| 48 | 8 | `di_ctime` | moves in *every* experiment — any metadata change bumps it |
| 56 | 8 | `di_size` | truncate 100000 vs 200000 |
| 64 | 8 | `di_nblocks` | 1 vs 4 blocks written |
| 72 | 4 | `di_extsize` (fsblocks) | extsize 65536 vs 131072 |
| 76 | 4 | `di_nextents` — **or `di_big_anextents` under `nrext64`** | 3 vs 1 extents |
| 80 | 2 | `di_anextents` | shortform vs 7 attribute extents; becomes pad under `nrext64` |
| 82 | 1 | `di_forkoff` (×8) | none / 1 xattr / 80 xattrs |
| 83 | 1 | `di_aformat` | none / shortform / extents |
| 84 | 4 | `di_dmevmask` | constant 0; named by position |
| 88 | 2 | `di_dmstate` | constant 0; named by position |
| 90 | 2 | `di_flags` | `+d` → 0x0080, `+S` → 0x0020, extsize → 0x0800 |
| 92 | 4 | `di_gen` | differs per inode, stable across records for one inode |
| 96 | 4 | `di_next_unlinked` | constant `ffffffff` — see below |
| 100 | 4 | `di_crc` slot | **always zero in the log**, while the disk copy holds a real checksum |
| 104 | 8 | `di_changecount` | 1 chmod vs 3; 3 vs 402 after 400 creations |
| 112 | 8 | `di_lsn` | `cycle << 32 \| basic block` of that inode's previous log record |
| 120 | 8 | `di_flags2` | bigtime 0x08, cowextsize 0x04, nrext64 0x10 |
| 128 | 4 | `di_cowextsize` | cowextsize 65536 vs 131072 |
| 132 | 12 | `di_pad2` | constant 0 |
| 144 | 8 | `di_crtime` | equals the parent directory's mtime at the moment the child was created |
| 152 | 8 | `di_ino` | matches `ilf_ino` in the format op ahead of it |
| 160 | 16 | `di_uuid` | two mkfs UUIDs came back exactly, in RFC byte order |

## Timestamps, both encodings proven

**bigtime** (`di_flags2 & 0x8`, the current default) — one `u64` of nanoseconds since
1901-12-13 20:45:52 UTC, i.e. unix minus 2³¹ seconds. Six controlled values decoded
exactly, including root's `mkfs` atime of `0x1dcd650000000000`, which is unix zero.

**legacy** (`mkfs -m bigtime=0`) — `i32` seconds then `u32` nanoseconds.

## Three things that would bite a writer

**A v4 filesystem logs 96 bytes, not 176.** `di_version` reads 2 and the structure simply
stops. Assuming 176 overruns into the next operation.

**`nrext64` moves two counters.** The data-extent count goes to offset 24 as a `u64` and
the attribute count to 76 as a `u32`, leaving 80 as padding. Gate on `di_flags2 & 0x10`.

**`di_crc` is zero in the log and real on disk.** A replayer must recompute it, and an
encoder must not copy the disk value across.

## Reproducing any of this

Delayed logging batches many transactions into one checkpoint. A setup phase of create,
chown, chmod and touch arrived as a **single 14-operation record** carrying each inode's
final state once — so "a chmod is five operations" holds only when the chmod is alone in
its checkpoint.

To isolate one operation: setup, `sync`, `umount`, remount, the one operation, `sync`,
`umount`.

## Left open

**Endianness is native, not provably little-endian.** Every observation was made on
arm64. The log core is very likely host-endian rather than fixed little-endian, and
nothing available here distinguishes the two. Settling it needs a big-endian host writing
a log.

**`di_next_unlinked` was never seen non-null**, and not for want of trying: 400 files
opened, unlinked while open, and synced produced 797 cores with `nlink == 0`, every one
holding `ffffffff`. The kernel appears to maintain the unlinked list through the inode
*buffer* item rather than the core. The field's position is certain — the sentinel value
and the on-disk agreement fix it — but its behaviour here is not.

**`di_flushiter`**, the last two bytes of the padding at 24 on v2 inodes, was only ever 0.

**Offsets 6–7, 84–89, 100–103 and 132–143** are proven constant zero, which is all a
writer needs; their names are assigned by position against the on-disk inode rather than
by watching them vary.
