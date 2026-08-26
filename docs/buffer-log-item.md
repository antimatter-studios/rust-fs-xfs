# The buffer log item

XFS logs a change to an allocation-group header, a B+tree block or a directory block by
recording *which bytes of which buffer changed*. That item — type `0x123c` — is what
allocation, create, unlink and rename all rest on.

Established by differential analysis against filesystems the kernel wrote, never from
implementation source. Corpus: 12 filesystems across 4 geometries, **434 buffer items**
parsed. Every structural invariant below held on all 434.

## Format op

All fields little-endian, which is worth stating because XFS is big-endian nearly
everywhere else.

| off | width | field | notes |
|----|----|----|----|
| 0 | u16 | `blf_type` = `0x123c` | `0x123b` is the inode item |
| 2 | u16 | `blf_size` | log ops this item occupies: 1 format op + N data ops |
| 4 | u16 | `blf_flags` | low 11 bits are flags; **bits 11..15 are a buffer-type code** |
| 6 | u16 | `blf_len` | buffer length in 512-byte basic blocks |
| 8 | i64 | `blf_blkno` | **absolute device address, in 512-byte basic blocks** |
| 16 | u32 | `blf_map_size` | dirty bitmap length, in 32-bit words |
| 20 | u32 × n | `blf_data_map` | one bit per **128-byte chunk** of the buffer |

Total size is `20 + 4 * map_size`. **No padding and no alignment** — the next op header
begins immediately. Op payloads are not even 4-byte aligned; a 2156-byte op was observed.

### `blf_blkno` is basic blocks, not what you would guess

This was established by varying the two plausible alternatives independently:

- At `bs=4096 sect=512`: AGI at blkno 2, bnobt 8, cntbt 16, inobt 24, finobt 32, SB 0.
- At `bs=1024`: AGI is **still** blkno 2 — so not filesystem blocks.
- At `sect=4096`: AGF is blkno **8**, AGI **16** — so not sectors either.

And it is absolute rather than AG-relative: headers in AG1/AG2/AG3 came out at
`agno * agblocks * bs / 512 + {1,2}`.

Each was cross-checked by reading the block at that address and confirming its magic —
`XAGI`, `AB3B`, `IAB3`, `XFSB` and so on. That is the strongest evidence available and it
agreed every time.

## The data ops

Exactly `blf_size - 1` of them, immediately after the format op.

**One data op per maximal run of consecutive set bits**, in ascending chunk order. Data
op *k* carries buffer bytes `[start_k * 128, (start_k + len_k) * 128)`, and its length is
exactly `len_k * 128`.

Chunk *c* is buffer offset `c * 128`; bit *c* lives in word `c / 32` at bit `c % 32`,
LSB-first within each little-endian word.

Two invariants held on all 434 items and are worth asserting in a parser:

```
op_len            == 20 + 4 * map_size
popcount(map)*128 == sum of the data-op lengths
```

### Watching the bitmap march

Appending one name at a time to a single-block directory, one checkpoint each:

```
chunks 0..5, 31                 data 768, 128
chunks 0, 5, 6, 30, 31          data 128, 256, 256
chunks 0, 5..7, 30, 31          data 128, 384, 256
chunks 0, 9..12, 29..31         data 128, 512, 384
chunks 0, 18..21, 28..31        data 128, 640, 512
```

Chunk 0 is the header and bestfree. The middle run marches forward as names are appended;
the tail run grows backwards from 31 as the leaf-entry array grows down from the end of
the block. Exactly what a 128-byte-chunk bitmap predicts, which is the point — the
behaviour was predicted before it was measured.

## What a replayer must do beyond copying bytes

**Recompute the checksum.** For the last item logging each block, the logged chunks equal
the on-disk bytes *except* the block's own CRC and LSN — those are stamped at write-out,
after logging. Observed on btree blocks, the superblock, directory blocks and the AGI.

**Do not replay an inode chunk verbatim.** Unlink writes four bytes at
`di_next_unlinked`, which dirties a whole 128-byte chunk — but that chunk carries a
*stale* inode image, `di_format` zero, timestamps zero, generation zero, while the disk
holds the live inode. 76 of 256 bytes differed, at exactly the fields that would be
uninitialised. Applying the chunk as-is would corrupt the inode.

That the mismatch exists is observed. That only the unlinked pointer should be applied is
the inference it forces.

## `blf_flags`

Low bits seen: `0x000`, `0x001`, `0x002`.

- `0x002` — always `blf_size = 1`, empty map, no data ops, and the address was a block
  being freed or reused. This is the cancel record: replay must let it suppress earlier
  items for the same block.
- `0x001` — only on inode-cluster buffers, and the case above.

`flags >> 11` is a buffer-type code that correlated 1:1 with the magic actually found at
`blkno` across all twelve images:

| code | block | code | block |
|---|---|---|---|
| 4 | any btree — `AB3B` `AB3C` `IAB3` `FIB3` `BMA3` share one code | 12 | `XDF3` dir free |
| 5 | `XAGF` | 13 | dir leaf1 |
| 7 | `XAGI` | 14 | dir leafN |
| 8 | `IN` inode (always with low bit `0x1`) | 15 | da node |
| 9 | `XSLM` remote symlink | 16 | attr leaf |
| 10 | `XDB3` single-block dir | 18 | `XFSB` superblock |
| 11 | `XDD3` dir data | 0 | only ever with CANCEL |

## Ops that cross a record boundary

An op that does not fit is truncated with `oh_flags = 0x04`, and the remainder becomes
op 0 of the next record with `oh_flags = 0x18`. Observed: a 3328-byte data op split as
2156 + 1172, concatenating back to `runlen * 128`.

This was the single apparent invariant violation in the audit, and it is not one.

## Transaction framing, for context

```
op  flags 0x01, length 0                      start
op  length 16   "TRAN" magic, type, tid, n    n = ops belonging to items
op  ...                                       the items
op  flags 0x02, length 0                      commit
```

`n` counts item ops, verified: 11 = 3 + 2 + 2 + 2 + 2.

## Left open

| question | what would settle it |
|---|---|
| discontiguous buffers with more than one map | fragment free space hard, then create a directory with `-n size=8192` whose blocks land non-contiguously |
| low flag bits `0x4`/`0x8`/`0x10` | presumed dquot flavours; mounting with all three quota types produced no dquot buffer items at all |
| buffer-type codes 1, 2, 3, 6, 17 and ≥19 | rt bitmap/summary, AGFL, dquot, rmap/refcount btrees — not exercised |
| whether `blf_blkno` is signed | always positive here; width and position are certain, signedness is not |

One negative result worth recording: **the AGFL never appeared as a buffer item** in any
workload, including heavy allocate-and-free churn. The AGF alone carried the freelist
head, tail and count.
