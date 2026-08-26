# What each operation logs

The op sequence XFS writes for each common filesystem operation, captured by doing one
operation at a time against filesystems the kernel wrote. Twelve operations, 53
transactions, 1694 ops.

This exists so a log writer knows what it must emit, and so the next person does not
have to measure it again.

## The framing fact that changes everything

**A log record does not contain a transaction. It contains a CIL checkpoint** — the
aggregate of every item dirtied since the last checkpoint, each logged once in its
final state.

Demonstrated rather than inferred:

- `touch a; touch b` between syncs produced **one** transaction id, **one** START,
  **one** COMMIT, and the parent directory inode logged **once** with a fork carrying
  *both* new entries.
- A create followed by a 4 KiB write came to 18 item ops, not 20: the inode is logged
  once as `CORE|DEXT` rather than twice.
- The transaction header's type field read `0x28` in **all 53 transactions**, across
  every operation. It does not identify the operation; it identifies the checkpoint.

For a driver performing one operation at a time this is good news: emit one checkpoint
per operation and the shapes below are exactly what is needed.

## Frame rules

| Field | Rule |
|---|---|
| `h_len` | the op bytes rounded up to 512 |
| maximum `h_len` | `h_size − 512` — 32256 by default, 15872 at `logbsize=16k` |
| `h_num_logops` | ops in *that record*, START and COMMIT included |
| `h_prev_block` | start block of the previous record |
| transaction header's count | logical item ops for the **whole checkpoint**, not the record |
| op payloads | cross 512-byte boundaries with no framing; only the cycle stamp intervenes |
| item order | **varies** between operations. Do not treat it as canonical |

## The shapes

Each is the operation's single checkpoint. A `sync` adds one or two 5-op checkpoints of
its own carrying the superblock buffer, plus the unmount record; those are not part of
the operation.

| operation | ops | items | what it touches |
|---|---|---|---|
| **rename in one directory** | **8** | **2** | two inodes only |
| truncate to zero | 11 | 4 | AGF, both free-space btrees, inode |
| write 4 KiB (allocating) | 12 | 4 | AGF, both free-space btrees, inode + extent |
| create empty file | 14 | 5 | AGI, both inode btrees, parent + new inode |
| unlink | 14 | 5 | same set, different order |
| mkdir | 15 | 5 | AGI, both inode btrees, parent + new inode |
| shortform to block directory | 23 | 9 | all of the above plus the directory block |

Two scale results worth knowing, because they say the shape is stable: a 250-extent
btree-format truncate is still 12 ops, and a 600 MB file spanning three allocation groups
is 23 — three AGF-plus-two-btrees triples rather than a different structure.

## Where to start

**Rename within a single directory**, constrained to: both names shortform, equal length,
target does not already exist.

- 8 ops, 2 items, **both inode items**. No buffer item, so no dirty-chunk bitmap, no
  128-byte alignment, no AG header or B+tree encoding.
- It touches **no allocator metadata at all**. Every other operation drags in three to
  seven buffer items across four distinct on-disk structures.
- It is a strict extension of the chmod shape the encoder already produces: the same
  START, transaction header, inode format, inode core, COMMIT skeleton, plus one more
  inode item and one fork-data op.
- It is self-checking. With an equal-length replacement name `di_size` does not change,
  so a mis-encoded fork shows up as a directory that reads back wrong rather than as a
  size inconsistency that could have several causes.

The runner-up, for the smallest *allocating* shape, is truncate-to-zero: it adds exactly
one new pattern — the AGF plus both free-space btrees, one 128-byte dirty chunk each —
and no fork-data op at all.

## Traps

**The fork stays big-endian inside a native-endian record.** The logged inode core is
native-endian, but a shortform directory or extent list logged alongside it is not: a
parent inode number read `00 00 00 80` for 128 while the core around it was
little-endian. Convert the core; leave the fork alone.

**`sb_logstart` is an AG-encoded block number**, not a linear one. The byte offset is
`((logstart >> agblklog) * agblocks + (logstart & (2^agblklog − 1))) * blocksize`. This
driver's `fsblock_offset` already does it.

**`mkfs` pre-stamps roughly the first 2 MiB of the ring** with headers carrying the
record magic, `h_cycle = 0` and `h_len = 0`. A scanner that trusts the magic alone finds
thousands of phantom records. Taking the greatest sequence number is enough to ignore
them, since cycle 0 loses to everything.

**The unmount record's `oh_len` is 0** even though eight bytes of payload follow it,
while `mkfs`'s own initial unmount record sets it to 8. Copy the behaviour rather than
rationalising it.

## Op flags

Over 1694 ops: `0x00` ×1562, START ×53, COMMIT ×53, `0x04` ×4, `0x18` ×4, unmount ×18.

`0x04` and `0x18` appeared **only ever as a pair at a record boundary**, splitting one
op across two records — a logged inode core cut 96 + 80 and 44 + 132. They are framing
for op payloads and have nothing to do with transaction semantics. Confirmed at a second
`logbsize`, where the split moved to the new boundary.

## Two item types beyond the inode and buffer items

**`0x123f`, inode-chunk creation.** 28 bytes, format op only, no data op, once per newly
allocated inode chunk. Its fields after the type and size prefix are **big-endian**,
unlike everything around them: allocation group, block, count, inode size, length,
generation. Self-consistent — 64 inodes × 512 bytes = 32768 = 8 × 4096.

**Buffer cancel.** A buffer item with size 1, empty bitmap, no data op, flag `0x2`,
emitted for a metadata block being freed. Replay must let it suppress earlier items for
the same block.

## What was not observed, and the limit of that

**No intent items.** No EFI/EFD, no BUI/CUI/RUI, in any of: a 16 KiB truncate, a
250-extent btree-format truncate, a 600 MB truncate spanning three allocation groups, or
unlinking a file that owned blocks. Only three item types ever appeared.

The likely explanation is that under delayed logging an intent whose completion lands in
the same checkpoint never reaches the log at all. That is **inferred**. No case could be
constructed that produced one, so where the boundary lies is unknown — a writer emitting
one self-contained checkpoint per operation appears never to need them, but that is an
observation over twelve operations rather than a proof.


## The rename record, read off a disk

Measured after the rest of this document, by renaming one file in a two-entry
short-form directory and dumping the checkpoint. It is here because it is the shape a
writer needs first, and because reading it settled three things guessing would not
have.

```
record: h_len 1024, 8 ops
  op 0  len   0  flags 0x01   START
  op 1  len  16               TRANS type 0x28, item-ops 5
  op 2  len  56               INODE ino 131  ilf_size 3  fields 0x0003  dsize 30
  op 3  len 176               the directory's core, mode 040755
  op 4  len  32               the directory's short-form entries
  op 5  len  56               INODE ino 132  ilf_size 2  fields 0x0001
  op 6  len 176               the renamed file's core, mode 0100644
  op 7  len   0  flags 0x02   COMMIT
```

8 ops, 2 items, 5 item operations — exactly the shape predicted above, which is some
evidence the table is right about the others too.

### `ilf_fields` has a second bit

`0x0003`, against `0x0001` for the file beside it. So `XFS_ILOG_DDATA` is **`0x02`**:
the item logs the data fork's inline contents, which follow the core as a third
operation. `ilf_size` reads 3 rather than 2 for exactly that reason — it counts the
item's operations.

### `ilf_dsize` is unpadded

30, while the operation carrying the fork is 32 bytes. Operations are padded to a
multiple of four and the padding is not part of the fork. A writer that sets `dsize`
from the operation's length is wrong by up to three bytes, in a structure where three
bytes is most of an entry's header.

### The fork stays big-endian inside a native-endian record

The short-form header read `02 00 00 00 00 80` — a count of 2 and a parent inode of
128, big-endian — surrounded by a little-endian core. Already known from the logged
core work; confirmed here in the one place it matters for a writer, which converts the
core and copies the fork.

### A rename appends rather than replacing

The part that would not have been guessed. Before:

```
aaaa  offset 0x60  ino 132
bbbb  offset 0x70  ino 133
```

after renaming `aaaa` to `cccc`:

```
bbbb  offset 0x70  ino 133
cccc  offset 0x80  ino 132
```

The old entry is removed and a new one appended with a **fresh, higher** offset — even
though the replacement name is the same length and would have fit exactly where the
old one was. The offsets increase through the list; they are readdir cookies, not byte
positions, and reusing one would hand the same cookie to a different entry.
