# What a read costs

Measured by `tests/read_path_cost.rs`, which counts **calls to the
device** rather than wall time. Wall time on a laptop with a warm page
cache says more about the laptop than the driver; call counts are
deterministic — the same image walked the same way makes the same calls
every time — so they can be compared across months and asserted on.

Wall time is printed beside them because it is what a user feels. It is
not what anything is judged by.

## 2026-09-06 — before any caching

Fixture: `xfsfeat-everything.img` (crc, finobt, inobtcount, rmapbt,
reflink), 132 directory entries, 64 files.

| shape | reads | bytes | reads/item |
|---|---:|---:|---:|
| walk — list every directory | 1155 | 1.20 MB | 8.8 |
| stat — resolve every file by path | 360 | 328 KB | 5.6 |
| read — read every file's contents | 522 | 754 KB | 8.2 |

### What the numbers say

**Nothing is remembered between operations.** `stat` resolves 64 paths
and spends 360 reads doing it. Every one of those paths walks the root
directory and each directory above its target, and the root is read
again for every single one — 64 times, from the device, for bytes that
did not change.

**The walk is the expensive shape**, at 8.8 reads per entry, because
listing a directory means the inode B+tree, then that inode, then its
block map, then its blocks — and the tree blocks at the top are shared
by every entry in the group and re-read for each.

**`read` costs less per item than `walk`** despite moving twice the
bytes, which is the useful surprise: the data itself is cheap to fetch
in large pieces, and the expense is the metadata around it. A cache
sized for metadata blocks should move `walk` and `stat` a long way and
leave `read` roughly where it is.

## 2026-09-06 — with a 512-block cache

Same fixture, same walk, `mount_with_cache(device, 512)` — which is what
`mount` now does by default.

| shape | reads | bytes | wall | vs before |
|---|---:|---:|---:|---|
| walk | 987 | 513 KB | 1609 µs | reads −15%, bytes −57%, time −58% |
| stat | 320 | 164 KB | 407 µs | reads −11%, bytes −50%, time −35% |
| read | 426 | 360 KB | 521 µs | reads −18%, bytes −52%, time −89% |

### The result is more interesting than the win

Bytes halve and wall time falls by more than half, but **the read count
barely moves**. Both numbers were kept for exactly this reason: one of
them alone would have told the wrong story.

The cause is in the cache's hit condition. It serves a read only when
that read is **exactly one aligned block**, and this driver almost never
reads that way:

> average read during an uncached walk: **1040 bytes**, against a block
> size of **4096**.

Inodes are read at inode size — 512 bytes — and the group headers at
sector size, also 512. Roughly three quarters of all reads are a
fraction of a block, miss the cache by construction, and go to the
device every time.

What did improve is real: a read that *is* block-sized now costs
nothing the second time, which is where the halved bytes and the
collapsed wall time come from. But the remaining 987 reads on a walk
are mostly sub-block reads of blocks the cache is already holding.

Serving those from the block already cached is tracked as
`antimatter-studios/rust-fs-core#28`. That is where the next large
reduction is, and this measurement is what says so rather than a guess
about where time goes.

## 2026-09-06 — with `am-fs-core` 0.2.8, which serves partial reads

Same fixture, same walk, same 512-block cache. The only change is
underneath: `CachingDevice` now serves a read from the blocks it falls
in, whatever its size or alignment, instead of only an exactly-aligned
whole block.

| shape | reads | bytes | wall | vs uncached | vs the aligned-only cache |
|---|---:|---:|---:|---|---|
| walk | 22 | 90 KB | 1564 µs | reads −98%, bytes −92% | reads −98% |
| stat | 0 | 0 | 310 µs | reads −100%, bytes −100% | reads −100% |
| read | 42 | 172 KB | 463 µs | reads −92%, bytes −77% | reads −90% |

### `stat` reaches zero, and that is the whole point

Resolving 64 paths from the root now costs **no calls to the device at
all**. Every directory those paths walk through was read during the
walk that preceded them and is still held; nothing has to be fetched
again. The uncached figure for the same work was 360 reads.

The walk falls from 1155 reads to 22 — a factor of 52. What is left is
the mount itself plus the first touch of each block; everything after
that is served from memory.

`read` keeps the largest residue, at 42, which is right: file contents
are the one thing that genuinely has to come off the device, and the
bypass rule sends a read spanning more than half the cache straight
through rather than evicting the metadata to hold it.

### What the previous entry got right, and what it under-called

The diagnosis was correct — three quarters of reads were sub-block and
missed by construction — but the size of the fix was under-called.
Serving partial reads was described as "the next large reduction". It
was not a reduction so much as the difference between a cache that
works and one that does not: **the aligned-only cache was removing 11%
of reads on `stat`; the same cache serving partial reads removes all of
them.**

The lesson is worth keeping: a cache whose hit condition does not match
the caller's access pattern reports plausible-looking small wins, and
the small wins are what make it look tuned rather than broken.

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

It skips without fixtures. Build them with
`scripts/vm-build-fixtures.sh`.
