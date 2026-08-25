
---

## 8. What the checksum experiments settled — 2026-08-25, later

The `h_crc = 0` lead from §7 is **dead**, and the three behaviours below are now known
rather than guessed. Each was observed by altering a real record and watching what the
kernel did.

| Record's `h_crc` | What the kernel does |
|---|---|
| Correct | Replays it normally |
| Wrong, non-zero | *"Torn write (CRC failure) detected at log block 0x42. Truncating head block from 0x7a."* — the record and everything after it is discarded, and the mount succeeds |
| Zero | **Mount fails**, `log mount/recovery failed: error -117` (EFSCORRUPTED) |

Two consequences, and the first is better news than it looks.

**Getting the checksum wrong is fail-safe.** A record with a bad checksum is treated as
a torn write and thrown away — the transaction does not happen, and nothing is
corrupted. That is a far gentler failure than the one assumed in §7, where the worry
was a record the kernel accepts but misreads. It substantially de-risks building a log
writer: the worst outcome of a bug is a write that silently did not take effect, which
a test catches immediately.

**Zero is the one value to avoid.** It is not "not computed" — it fails the mount
outright, which is worse than a wrong value. Any writer must compute something, and
must never leave the field clear.

Note also that a *clean* log is not gated on the checksum at all: both a zeroed and a
corrupted `h_crc` on the head unmount record still mounted. Only replay verifies it.
So the checksum matters exactly when a record has to take effect.

### The span is still unknown, and the search was exhaustive

Two systematic sweeps against three real records, both negative:

- **Every contiguous span** beginning at the record start or the data start, in 4-byte
  steps up to the whole 32 KiB record, against four finalisations of the stored value
  (as-is, inverted, byte-swapped, both) — and with the cycle stamp both applied and
  undone.
- **Every subset of eight header fields** being zero at checksum time (`h_cycle`,
  `h_len`, `h_lsn`, `h_tail_lsn`, `h_prev_block`, `h_num_logops`, `h_cycle_data`,
  `h_size`), again stamped and unstamped — 512 combinations.

CRC32C itself is not in question: the same routine verifies superblocks, inodes,
directory blocks and B+tree blocks in this crate.

**So the buffer checksummed is not the record as it sits on disk.** The likely
explanation is that it covers the in-memory iclog before it is written — which may
include bytes that never reach the disk, or reach it rearranged. Cracking it needs a
different technique than sweeping: most promising is to make the filesystem produce a
record whose content is small and fully known, and work from that.

This is a research problem, not a task with an estimate, and it should be picked up as
one.
