# Arithmetic-overflow audit of disk-read values — 2026-09-09

**Scope:** `src/`, 34 files, production code only (every `#[cfg(test)]`
module excluded). Bare `+`, `-`, `*` and `<<` applied to values that
originate on disk.

**Result:** one defect found and fixed (`agfl.rs`), six sites examined
and shown bounded, one out-of-scope site recorded for later. The method
and its limits are written down below, because the first version of it
produced a clean bill of health that was wrong.

This is the follow-on to #121. That issue added a debug-profile CI job,
which makes an overflow *observable*; it does not establish that none
exists. This is the pass that looks.

---

## The decode boundary

This crate has one, and it is what makes the audit tractable. Every
on-disk integer is read through `src/endian.rs` — `be16`, `be32`, `be64`
and `le32`. Nothing else calls `from_be_bytes` on disk data. So "a value
that came off a disk" is decidable by following those four functions.

## Method, and the false negative that changed it

**First attempt — file-local, binding-based taint.** Track `let x =
be32(...)` within a file, then look for arithmetic on `x`. It reported
**7 high-risk sites, all safe.**

That result was worthless, and there was a control available to prove
it: issue #113 names `rmap::write_keys` as "exactly the shape this audit
is for" —

    src/rmap.rs:206   u64::from(r.startblock) + u64::from(r.blockcount) - 1
    src/rmap.rs:208   r.offset + u64::from(r.blockcount) - 1

**The scan found neither.** `r` is an `Rmap` decoded in another function;
the values reach the arithmetic as struct fields, not as local bindings,
and file-local tracking cannot see across that boundary.

**The taint boundary is the struct field, not the local binding.** The
second scan collects, crate-wide, every field or binding name ever
assigned from a decode — 87 of them — and then looks for arithmetic on
any of those names or on `.name` accesses. It finds `rmap.rs:206` and
`:208`, and the scan now fails loudly if it does not.

That is the general lesson and it is worth more than the finding:
**a scan of this kind needs a known positive before its negatives mean
anything.** Without #113 to check against, this audit would have
reported "all sites bounded" and been believed.

The second scan yields **126 candidate sites**. It is deliberately
over-broad — some tainted names (`at`, `len`, `size`, `count`) are
common words that collect false positives — because for an audit the
cheap error is a site that turns out fine.

## What was found

### FIXED — `src/agfl.rs`, the free-list span

    (agf.fllast + capacity as u32 - agf.flfirst) % capacity as u32 + 1

`agf_flfirst` and `agf_fllast` are `be32` straight off the disk
(`src/ag.rs:253-254`) with nothing bounding them. This expression was
both their first use **and** their validation — so it had to survive the
values it existed to reject.

At 119 entries a sector, any `fllast` above `u32::MAX - 119` overflows
the addition.

- **Debug:** `attempt to add with overflow`, panic, at `agfl.rs:127`.
- **Release:** wraps. The wrapped span is then compared against
  `flcount` — and **when the wrap lands on the count, the corrupt header
  is accepted.** `0xFFFF_FFFF + 119` wraps to 118; with `flfirst` 0 that
  is a span of 119; a full list has `flcount` 119. So an AGFL whose last
  index points four billion entries past a 119-entry ring passes
  validation.

The release case is the one that matters, and it is why the fix is not
"add `checked_add`". The indices are now **bounded before the span is
computed** — a ring index at or beyond the ring is wrong on its own
terms and can be said so first — after which the addition cannot
overflow and the subtraction cannot underflow.

Test: `a_free_list_header_with_wild_indices_is_refused_not_wrapped`.
With the bound removed it fails in **both** profiles — panicking in
debug, and in release reporting that the wrapped span was accepted.

### EXAMINED AND BOUNDED

| site | operation | why it is safe |
|---|---|---|
| `superblock.rs:665` | `(agblocks - 1)` | `agblocks == 0` rejected eight lines above, at `:657` |
| `superblock.rs:676` | `agcount * agblocks` | both widened `u32`→`u64` first; `u32::MAX²` < `u64::MAX` |
| `superblock.rs:737` | `blocksize << dirblklog` | `validate()` filters on `checked_shl`; `parse()` is the only fallible constructor and calls it |
| `ag_btree.rs:352` | `node.level - 1` | `if node.level == 0 { … continue }` at `:320` |
| `bmbt.rs:338` | `root.level - 1` | `parse_root` rejects level 0 explicitly at `:131` |
| `bmbt.rs:362` | `node.level - 1` | in the `else` of `if node.level == 0` |
| `inode_btree.rs:450` | `node.level - 1` | `if node.level == 0 { … continue }` at `:420` |
| `agfl.rs:52` | `sectsize - header` | `sectsize` validated to `512..=32768` at `superblock.rs:451` |
| `extent.rs:140` | `startblock << BLOCKCOUNT_BITS` | `startblock > STARTBLOCK_MAX` rejected at `:125` |
| `fs.rs:592` | `inode.size - offset` | `if offset >= inode.size { return Ok(0) }` at `:576` |

Three of these are bounded by a check *in a different function* from the
arithmetic (`superblock.rs:737`, `extent.rs:140`, `fs.rs:592`). They are
safe as the code stands; they are also the ones that would stop being
safe without anybody editing the arithmetic.

### RECORDED, NOT FIXED

- **`rmap.rs:206` and `:208`** — the `- 1` on a `blockcount` that
  `decode` does not validate. Already filed as **#113**, which is a
  separate accepted issue with its own scope (it is also about the high
  key being one the kernel does not build). Not fixed here so as not to
  pre-empt it; it is the audit's second confirmed positive.
- **`dir_block.rs:240`**, `u::tag(free_len)` = `free_len - 2`. Out of
  this audit's stated scope: `free_len` is `index_start - entries_end`,
  both crate-derived while *building* a block, not read from disk. It is
  guarded by `if free_len > 0`, which admits `free_len == 1`; a
  directory free region is always a multiple of 8, so 1 is not
  reachable today. Worth its own issue rather than a silent widening.

## What this pass does not cover

- **Casts.** `as usize` / `as u32` truncation is a different defect
  class and was not audited. There are many.
- **`usize` arithmetic on lengths** derived from disk *counts* —
  e.g. `node.body + numrecs * record_len`. These appear in the 126
  candidates and were read, but they are bounded by an explicit
  `end > buf.len()` check immediately after, in every case examined
  (`ag_btree.rs:321,341`, `inode_btree.rs:421,440`, `bmbt.rs:273,288`).
  On a 64-bit host `u16 × small constant` cannot overflow `usize`, so
  the check is the real guard and it is present.
- **The other eleven repositories.** #140 scopes this to `rust-fs-xfs`
  deliberately, to see what it costs first. It cost one real defect out
  of 126 candidates, and the single most valuable output was the
  methodological one: **the field-level taint boundary, and the need for
  a known positive to validate the scan.** A sibling repo running the
  binding-local version of this scan would get a clean report and learn
  nothing.
