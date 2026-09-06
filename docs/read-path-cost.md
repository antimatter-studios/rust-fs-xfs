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

## How to take the measurement again

```sh
cargo test --release --test read_path_cost -- --nocapture
```

It skips without fixtures. Build them with
`scripts/vm-build-fixtures.sh`.
