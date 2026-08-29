# Human-code review — 2026-08-28

> **This is analysis only. No code was modified.** No file in `src/`, `tests/`,
> `examples/` or `scripts/` was touched, no branch was created and nothing was
> committed. The only change to the working tree is this document. The
> `human-code` skill asks for confirmation before implementing anything; that
> confirmation has not been given, so this stops at the triage.

**Scope:** the whole crate — `src/` (18,229 production lines across 29 files,
excluding `#[cfg(test)]` modules), with `tests/`, `examples/` and the crate
manifest read for context.

**Counts:** **43 findings** — **8 High**, **20 Medium**, **15 Low**. **0 fixed**,
**43 open**. Nine further candidates were examined and are *not* raised; they are
listed under [Considered and not raised](#considered-and-not-raised) with the
reason, because two of them contradict the previous review and one of those
contradictions matters.

**Baseline:** `cargo test` — 313 passed, 0 failed, 12 ignored, across 27 suites.
`cargo fmt --check` clean, `cargo clippy --locked --all-targets -- -D warnings`
clean.

---

## The shape of it

This crate has an explicit, written discipline about on-disk constants. It is
stated in `AGENTS.md`, restated in `src/format/mod.rs:1-70`, restated again in
`src/endian.rs:1-30`, and it is the thing the previous review
(`docs/code-quality-review-2026-08-25.md`) singled out as having held:

> **No unnamed multi-digit offsets.** Every structure has a documented `offsets`
> module. This was a deliberate change and it has not regressed.

That was true of the read path and it is still true of the read path. It is
**not** true of the write path, which is roughly 6,000 production lines that did
not exist when that sentence was written. Six of the eight High findings below
are the same failure in six places: a canonical, well-documented table exists,
the write path does not use it, and a private copy was made instead. In four of
those cases the module that holds the canonical table carries a doc comment
explicitly forbidding the copy that was then made.

That is the theme. The individual findings matter, but the reason there are
eight Highs rather than two is that one habit stopped being applied at a module
boundary, and nothing in the build catches it.

The second theme is smaller and easier: **four comments now assert the opposite
of what the code does**, and in one case the comment instructs a future reader to
make a fix that has already been made somewhere else.

On the question the brief asked — *is the dense on-disk offset handling explained
or merely present?* — the answer is overwhelmingly **explained**. `src/format/`,
`src/endian.rs`, `src/buf_write.rs:30-46`, `src/log.rs:147-161` and
`src/group_write.rs:69-84` are the best-documented on-disk code in this crate
family, and several carry numeric provenance ("pinned by 3 against 402 after 400
creations", "verified against all 24 checksummed records across four
filesystems", "the AGFL never appeared as a buffer item"). The magic-number
findings below are the exceptions, and they are flagged precisely *because* the
surrounding standard is so high — each one is a place where the crate does not
meet its own bar, not a place where the bar is unreasonable.

---

## Findings

### High

Severity **High** means the confusion could hide a bug, per the skill's
definition. Two of these are not merely confusing — H2 describes a reachable
defect and H5 describes an unmarked behavioural divergence.

---

#### H1 — `rename_in_directory` is the only journalled write with no v5 gate

**`src/dir_write.rs:78-82`**

Every other journalled entry point refuses a v4 filesystem by name:

| entry point | v5 gate |
|---|---|
| `src/create.rs:334-338` | `"creating writes v5 metadata; a v4 filesystem is not supported"` |
| `src/unlink.rs:125-129` | `"removing writes v5 metadata…"` |
| `src/truncate.rs:83-87` | `"truncating writes v5 metadata…"` |
| `src/file_write.rs:145-148` | `"writing allocates v5 metadata…"` |
| **`src/dir_write.rs:79-82`** | **none** |
| `src/log_write.rs:575-578` | none |

`rename_in_directory` also deviates in form: it uses `if self.writable.is_none()`
and then recovers the handle sixty lines and one full parse later with
`self.writable.as_ref().expect("checked above")` (`src/dir_write.rs:143`), where
the other four use `let Some(device) = … else`. `Filesystem::mount_rw`
(`src/fs.rs:152`) does not refuse v4, and `log_dinode_from_disk`
(`src/log_write.rs:299-303`) accepts version 1 and 2 cores.

Whether the missing gate is deliberate is **unanswerable from the code**. There
is no comment either way. `tests/rename_oracle.rs` has five tests, all against
the v5 default fixture; the `xfs-nocrc` v4 fixture exists in `.vm-share/` and no
rename test uses it.

**Coverage:** `tests/rename_oracle.rs` (5 tests, all v5). `src/dir_write.rs` has
no in-file unit tests.

---

#### H2 — every write entry point burns the mount's one checkpoint before it validates anything

**`src/fs.rs:98-109`**, called first at **`src/create.rs:330`**,
**`src/unlink.rs:121`**, **`src/truncate.rs:79`**, **`src/file_write.rs:140`**,
**`src/dir_write.rs:79`**, **`src/log_write.rs:575`**

`begin_checkpoint` is a one-shot consuming swap:

```rust
pub(crate) fn begin_checkpoint(&self) -> Result<()> {
    if self.checkpointed.swap(true, Ordering::SeqCst) {
        return Err(Error::UnsupportedFeature(
            "this mount has already written a checkpoint, and a second would be built \
             from a disk that does not yet reflect the first — mount again after the \
             log has been replayed".into(),
        ));
    }
    Ok(())
}
```

`checkpointed` is set to `false` only at mount (`src/fs.rs:130`, `src/fs.rs:164`)
and is never reset. And in all six entry points `begin_checkpoint()?` is the
**first statement in the function** — before the read-only check, before the v5
check, before the name-validity check, and before every inode-shape refusal.

So an operation that is *refused* consumes the mount's only checkpoint. Calling
`create_file` with a name that already exists returns `AlreadyExists`
(`src/create.rs:360`) and burns the token; the next, entirely legitimate
`create_file` on the same handle returns *"this mount has already written a
checkpoint, and a second would be built from a disk that does not yet reflect the
first"* — describing a write that never happened. Every refusal in the write path
does this: `ReadOnly`, the v4 refusal, `NotADirectory`, `NotAFile`, the
non-`Local` fork refusal, the realtime refusal, the tree-depth refusal, the
no-free-inode refusal.

The one-checkpoint-per-mount rule is deliberate and documented (`README.md`,
"A mount writes at most one checkpoint", and `src/fs.rs:78-97`). Spending it on
a refusal is not what that rule describes.

The fix is to move `begin_checkpoint()?` below the guards — but note it has to
move below *all* of them, and the guards are currently interleaved with real
work (`src/create.rs:346-361` reads and parses the parent directory before the
last refusal). That is the same tangle H6 and M3 are about.

**Coverage:** **none.** `checkpointed` appears in no file under `tests/`.
`tests/fs_refusals.rs` (13 tests) is entirely read-path.

---

#### H3 — the inode-core offset table is declared in seven places, and the documented one is unused

**canonical: `src/inode.rs:36-90`** — copies at **`src/create.rs:140-148`**,
**`src/create.rs:192-194`**, **`src/unlink.rs:70-78`**,
**`src/file_write.rs:87-94`**, **`src/group_write.rs:52-60`**,
**`src/dir_write.rs:417`** and **`src/dir_write.rs:434`**, plus
**`src/format/log_items.rs:650-720`** and two test-local pairs
(`src/create.rs:695-696`, `src/unlink.rs:389-390`).

`inode::offsets` is fully documented, and its doc comment states the exact
hazard:

```rust
/// Byte offsets within the on-disk inode (`xfs_dinode`).
///
/// Named for the same reason the superblock's and the AG headers' are.
/// The inode core is the densest structure in the format — several
/// fields change position depending on feature flags — so an unnamed
/// literal here is especially hard to check by eye.
```

`grep -rn 'inode::offsets' src/` returns **one** hit, at `src/inode.rs:369`,
inside `inode.rs` itself. The write path never reaches for it.

Counting one field: `di_size = 56` is separately declared at `src/inode.rs:61`,
`src/create.rs:145`, `src/unlink.rs:75`, `src/file_write.rs:89`,
`src/group_write.rs:54`, `src/dir_write.rs:417` and
`src/format/log_items.rs:679` — **seven declarations of one offset**.
`di_changecount = 104` has six. Offset 24 has five, under **two different
names**: `BIG_NEXTENTS` in `inode.rs`, `NEXTENTS64` everywhere else.

The least discoverable copy is `src/create.rs:192-194`, which puts three of them
*inside a function body*:

```rust
fn set_nextents(core: &mut [u8], count: u64) {
    const NEXTENTS: usize = 76;
    const NEXTENTS64: usize = 24;
    const FLAGS2: usize = 120;
```

Nothing links any of these to any other. They agree today; there is no mechanism
by which they would continue to.

**Coverage:** the offsets are exercised transitively by
`tests/create_replay_oracle.rs`, `tests/unlink_replay_oracle.rs`,
`tests/truncate_replay_oracle.rs`, `tests/file_write_replay_oracle.rs` and
`tests/log_dinode_oracle.rs` — all of which are kernel-replay oracles, so a
divergence would be caught, but only on the exact shapes those fixtures cover.

---

#### H4 — the log record header layout exists in three copies, and the canonical one has no importers

**canonical: `src/format/log_items.rs:123-215`** — copies at
**`src/log.rs:44-135`** and **`src/log_write.rs:218-248`**

`format::log_items::rec_header` is the reference table: thirteen offsets, each
with its C field name, its width, and in several cases the observation that
established it. `grep -rn 'rec_header' src/ tests/` finds exactly one importer —
`src/log_write.rs:68`, for `XLOG_VERSION_2` alone. Nothing imports its
`offsets`.

`src/log.rs:103-118` is a second copy: ten of the thirteen fields, undocumented
except for two, **out of offset order** (`CRC: 32` is declared after
`NUM_LOGOPS: 40` and `FMT: 300`), and missing `TAIL_LSN` (24), `PREV_BLOCK` (36)
and `CYCLE_DATA` (44). It carries a paraphrase of the canonical module's own
justification:

```rust
/// The fields between the ones this module reads are named too, even
/// where nothing consults them: an offset can only be checked against
/// the format documentation when its neighbours are there to be counted
/// off against, and `h_lsn` at 16 is only obviously right if `h_cycle`,
/// `h_version` and `h_len` are visible above it.
#[allow(dead_code)]
mod offsets {
```

Three of the neighbours are absent and the order is scrambled, so you cannot
count 16 → 32 → 40 off against anything. The rationale describes the module it
was copied from, not this one.

`src/log_write.rs:218-248` is a third copy, written as thirteen **bare
literals** — `0, 4, 8, 12, 16, 24, 32, 36, 40, 44, 300, 304, 320` — with no
offset table and no field-name comments, in a file that uses the named-offset
idiom correctly sixty lines earlier (`src/log_write.rs:142`, `use
…inode_log_format::offsets as at;`).

Both module docs prohibit this in writing:

- `src/format/log_items.rs:15-18` — *"Where a constant is already named
  elsewhere in the crate, the name and value here are identical on purpose; this
  must not become a second, divergent copy of the format."*
- `src/log_write.rs:56-58` — *"Re-exported rather than restated: a magic number
  that appears twice is a magic number that can disagree with itself."*

And `src/log.rs:45` already does `pub use crate::format::log_items::BBSIZE;` —
the re-export idiom is known here and was applied to exactly one constant before
the rest were hand-copied.

**Coverage:** `tests/log_checksum_oracle.rs` (2), `tests/log_encode_oracle.rs`
(1), `tests/log_oracle.rs` (2), `tests/log_replay_oracle.rs` (2). These compare
encoder output against kernel-written records, so a divergence between copies
would be caught — but only for the fields those records exercise, and nothing
compares the three tables to each other.

---

#### H5 — btree node parsing is triplicated, and one copy silently omits an identity check

**`src/alloc_btree.rs:151-158`, `:174-270`, `:295-362`**
**`src/inode_btree.rs:234-241`, `:243-333`, `:341-402`**
**`src/bmbt.rs:102-110`, `:157-237`, `:284-341`**

The same private `struct Node { level, numrecs, body, maxrecs }` is declared in
all three files. `alloc_btree::parse_block` and `inode_btree::parse_block` are
near-verbatim: same header sizing, same magic check, same CRC/UUID/owner/blkno
sequence, same level check, same `maxrecs` computation, same `Node`
construction. They differ in the `Order`/`Which` enum, the `what:` string in the
error, whether `space / per` is spelled inline or via a `maxrecs` helper — and
in `alloc_btree` having explanatory comments that `inode_btree` does not. The
two `walk` functions are the same again, differing only in how a leaf record is
decoded.

The part that matters: **`bmbt::parse_block` never verifies the block's
self-recorded address**, and the other two do.

```rust
// src/alloc_btree.rs:89-97          // src/bmbt.rs:72-80
mod offsets {                        mod offsets {
    pub const MAGIC: usize = 0;          pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;          pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;        pub const NUMRECS: usize = 6;
    pub const BLKNO: usize = 16;         /// v5 only, from here down.
    pub const UUID: usize = 32;          pub const UUID: usize = 40;
    pub const OWNER: usize = 48;         pub const OWNER: usize = 56;
    pub const CRC: usize = 52;           pub const CRC: usize = 64;
}                                    }
```

`bmbt` has no `BLKNO` constant, and `grep -in 'blkno\|block address' src/bmbt.rs`
returns nothing at all. `alloc_btree.rs:225-234` and `inode_btree.rs:290-299`
both perform the check and both explain why. The long-form v5 btree header does
carry `bb_blkno` (at 24, by the same +8 shift that moves `UUID` from 32 to 40),
so the field is there to check.

`AGENTS.md` states the rule this omission sits against:

> On v5, verify both the CRC **and** the self-describing identity fields (UUID
> and owning AG). The checksum catches corrupted bits; the identity fields catch
> an intact block that came from the wrong place.

The readability failure is the one that makes this High: three near-identical
parsers where the differences are unmarked, so a reader has no way to tell an
intentional divergence from an oversight. I could not determine which this is,
and neither can the next reader.

**Coverage:** `tests/alloc_btree_oracle.rs` (1), `tests/inode_btree_oracle.rs`
(2), plus 11/4/10 in-file unit tests for `alloc_btree`/`inode_btree`/`bmbt`.
Nothing tests the misdirected-block case for `bmbt`.

---

#### H6 — `write_into_empty_file` open-codes `allocate_in_group`, which already exists

**`src/file_write.rs:195-313`** vs **`src/group_write.rs:258-356`**

`diff` of the two ranges: the divergence is two words of error prose
(`"splitting across extents"` vs `"splitting a file across extents"`), one
longer CRC comment, three lines of comment present in one and not the other, and
whether the three `changed_chunks` results are bound to named locals or pushed
into a `Vec`. The AGF read, the tree-depth guard, both root reads, the `numrecs`
parse, first-fit selection, `alloc_extent`, the capacity guard, the `by_count`
sort, both `rebuild_leaf` calls and the `freeblks`/`LONGEST` updates are
identical text.

`allocate_in_group` is `pub(crate)` and has exactly one caller,
`src/create.rs:251`.

And `src/group_write.rs:3-7` — the module doc of the file that holds the helper
— asserts the opposite:

```rust
//! Freeing an extent and allocating one are the same piece of work in
//! opposite directions: read the group header and the two free-space
//! tree roots, change the records, and log which bytes of each block
//! changed. What differs between them is only which way the records
//! move, so everything else lives here rather than twice.
```

It lives three times: `src/group_write.rs:258-356`,
`src/file_write.rs:195-313`, and `src/truncate.rs:137-220` in the free
direction. A reader who trusts that paragraph will not go looking for the
copies.

**Coverage:** `tests/write_oracle.rs` (4), `tests/file_write_replay_oracle.rs`
(2), `tests/truncate_oracle.rs` (3), `tests/truncate_replay_oracle.rs` (1),
`tests/inode_alloc_oracle.rs` (3). Good coverage of the behaviour; none of it
would notice the three copies drifting apart, because each is tested through its
own entry point.

---

#### H7 — `emptied_core` names two different live functions with different meanings

**`src/unlink.rs:85`** and **`src/group_write.rs:179`**

| | `unlink::emptied_core(raw)` | `group_write::emptied_core(raw, v5)` |
|---|---|---|
| zeroes | `di_mode`, `di_nlink`, `di_size` | `di_size`, `di_nblocks`, `di_nextents` |
| bumps | `di_gen`, `di_changecount` | `di_changecount` |
| leaves alone | `di_nblocks`, `di_nextents` | `di_mode`, `di_nlink`, `di_gen` |
| means | *this inode is free again* | *this file exists but holds nothing* |

Neither is a superset of the other. Both are live —
`src/unlink.rs:263` and `src/truncate.rs:222`. And `src/unlink.rs` imports
`changed_chunks` and `rebuild_inode_leaf` from `group_write`, the very module
exporting the other one, so both names are in scope in the same file.

Swapping them produces either a freed inode that still looks allocated, or a
truncated file whose inode has been deallocated. The shared name is the only
thing between the two. `freed_core` and `truncated_core` would end it.

Related: `src/truncate.rs:222` calls `emptied_core(&raw, true)` — an unlabelled
positional `bool` meaning "v5", at a call site that already refused non-v5 at
`src/truncate.rs:83-87`, so the argument can never be `false` there.

**Coverage:** `src/unlink.rs` has 2 in-file tests, `src/group_write.rs` has 4;
both are exercised by their replay oracles. Nothing would catch a swap except
the oracle, and only for the shapes it covers.

---

#### H8 — four comments assert the opposite of what the code does

**a. `src/create.rs:43-44`** — module doc, under *"What it will not do — each is
refused by name rather than attempted"*:

```
//! - a parent that is not a short-form directory, or one with no room
//!   left in its inode for another entry;
```

The second clause is false. `src/create.rs:439-450` handles exactly that case by
calling `convert_to_block_form`. It is now the feature, not the refusal. A
reader trusting the doc concludes the conversion branch is unreachable.

**b. `src/create.rs:46`** — same list:

```
//! - inode trees more than one level deep, or a root with no room;
```

The depth check exists (`src/create.rs:374-382`). The "root with no room" check
**does not exist anywhere in `create`** — the capacity refusal is only in
`src/unlink.rs:229-236`. Given the list's own framing ("refused by name"), this
promises a guard that is not there.

**c. `src/create.rs:535-537`** — two comment blocks, one true, one false,
adjacent, with an orphaned fragment welded between them:

```rust
        // Three operations for the parent — format, core and entries —
        // and two for the new inode, which logs no fork of its own.
        // The new inode's own fork operation, when it has one.
        let new_dsize = new_fork.len();
        …
        // Three operations for the parent — format, core and entries —
        // and two or three for the new inode, depending on whether it
        // has a fork. That one operation is the whole difference between
        // a create's fourteen and a mkdir's fifteen.
        let new_ops = if new_dsize == 0 { 2 } else { 3 };
```

Lines 535-536 are the pre-`mkdir` version and are directly contradicted six
lines below. Line 537 is a sentence fragment from the newer block.

**d. `src/dir.rs:218-236`** — the most consequential of the four, because it
tells the reader to make a fix that has already been made.

```rust
/// This is deliberately not [`Superblock::has_ftype`]. That method tests
/// only the v5 incompatible feature bit, which is where v5 filesystems
/// advertise the feature — but a **v4** filesystem advertises it in
/// `sb_features2` instead …
/// Fixing [`Superblock::has_ftype`] to match belongs in that module, not
/// this one.
fn dir_has_ftype(sb: &Superblock) -> bool {
    if sb.has_ftype() { return true; }
    sb.versionnum & version_flags::MOREBITSBIT != 0 && sb.features2 & SB_VERSION2_FTYPE != 0
}
```

`Superblock::has_ftype` (`src/superblock.rs:636-641`) **already tests both
conditions**, and its own doc explains the v4 case. Its second clause is
`versionnum & MOREBITSBIT != 0 && features2 & features2_flags::FTYPE != 0`, and
`features2_flags::FTYPE` (`src/superblock.rs:171`) is `0x0000_0200` — the same
value as `dir::SB_VERSION2_FTYPE` (`src/dir.rs:81`) and as
`format::dir::XFS_SB_VERSION2_FTYPE` (`src/format/dir.rs:314`), a third copy of
the same constant.

So `dir_has_ftype`'s extra clause is only reached when `sb.has_ftype()` returned
false, which is exactly when that clause is also false. **The two functions are
provably equivalent**, `dir_has_ftype` is redundant, and its thirteen-line doc
comment asserts a difference that no longer exists while directing the reader at
a module that has already been corrected.

I flag this at High rather than Medium because the doc is persuasive and
specific — it names the fixture and the symptom — and a reader who believes it
will distrust `Superblock::has_ftype` and may "fix" it into something wrong.

**Coverage:** (a)–(c) are comments; the code around them is covered by
`tests/create_replay_oracle.rs` (4) and `tests/endtoend_oracle.rs` (4). For (d),
`src/dir.rs:1640-1643` tests `dir_has_ftype` across four superblock shapes; no
test asserts the relationship between the two functions.

---

### Medium

Severity **Medium** means it slows comprehension without hiding a bug.

---

**M1 — `src/dir.rs:83-210` duplicates `src/format/dir.rs:467-1023`.** All 31
constants in `dir.rs`'s `pub mod offsets` exist in `format::dir::offsets` with
byte-identical values (no drift today; `format::dir` has 63 and adds `const fn`
accessors). `pub const fn da_counts` is verbatim in both (`src/dir.rs:207-209`,
`src/format/dir.rs:1020-1022`). Directly above it, `src/dir.rs:44-48` says the
constants are *"defined once, in [`crate::format::dir`] … so that a value can be
checked against its neighbours rather than against a second copy that happens to
agree."* Commit `98e034b` moved the constants and left the offsets.
`src/dir_block.rs:62` already imports the canonical module; `src/dir.rs` and
`tests/dir_oracle.rs:705` use the local one. Deleting the local module and
re-exporting takes `dir.rs` from 1,232 production lines to ~1,104 mechanically.

**M2 — the write-path preamble and the transaction bracket are copied four times
each.** Preamble (`begin_checkpoint` + writable + v5) byte-identical at
`src/create.rs:330-338`, `src/unlink.rs:121-129`, `src/truncate.rs:79-87`,
`src/file_write.rs:140-148`, differing only in the verb inside the error string;
two further entry points deviate (H1). The `vec![Op { flags: XLOG_START_TRANS
… }, Op { flags: 0, data: trans_header(…) }]` opener is byte-identical at
`src/create.rs:562-571`, `src/unlink.rs:301-310`, `src/truncate.rs:235-244`,
`src/file_write.rs:326-335`, and the `XLOG_COMMIT_TRANS` tail at
`src/create.rs:625-628`, `src/unlink.rs:339-342`, `src/truncate.rs:256-259`. Ten
lines of boilerplate bracketing every transaction. A `begin_write(verb)` helper
would also make H1's two deviations either impossible or explicit.

**M3 — `src/create.rs:366-391` ≡ `src/unlink.rs:174-199`, 26 consecutive
byte-identical lines**, covering AG geometry, the AGI read, `Agi::parse`, the
two-tree depth refusal, the `read` closure and `walk_from_agi`. One concept —
"open this group's inode trees" — copied wholesale.
`src/create.rs:513-531` ≡ `src/unlink.rs:278-296` is 19 more (the three
`changed_chunks` calls).

**M4 — the free-space leaf update is triplicated.** `src/file_write.rs:222-256`,
`src/group_write.rs:288-319`, `src/truncate.rs:164-182`. Head and tail are
byte-identical in all three — the `numrecs` big-endian parse, the
`leaf_records` call, and the capacity guard down to its error string. Only the
middle differs, and only in direction: allocate (first-fit + `alloc_extent`) vs
free (`free_extent` per extent). Roughly four lines of real difference wrapped in
thirty lines of identical scaffolding. `src/unlink.rs:233` carries a fourth copy
of the guard message, for the inode btree.

**M5 — AG header addressing is open-coded seven times, and the sector index is
unnamed.** `let ag_start = u64::from(agno) * u64::from(self.sb.agblocks) *
block;` at `src/create.rs:367`, `src/file_write.rs:192`,
`src/group_write.rs:260`, `src/truncate.rs:138`, `src/unlink.rs:175`.
`let ag_bb = ag_start / BBSIZE as u64;` at the same five files. The AGF is
`ag_start + sector` (`src/file_write.rs:196`, `src/group_write.rs:263`,
`src/truncate.rs:142`); the AGI is `ag_start + 2 * sector`
(`src/create.rs:371`, `src/create.rs:515`, `src/unlink.rs:179`,
`src/unlink.rs:280`). The bare `2` — "the AGI is the group's third sector" — is
the only thing distinguishing writing an AGI from writing an AGF, and it appears
four times with no constant and no comment at any site. `src/ag.rs:4-7` states
the sector layout in prose; `src/truncate.rs:205-207` does comment its AGF
arithmetic, so the crate's own standard is to explain this.

**M6 — the item-operation tallies end in bare addends that nothing checks.**
`src/create.rs:554-560` (`+ 3 + new_ops`), `src/unlink.rs:298` (`+ 3 + 2`),
`src/file_write.rs:323` (`+ 3`), `src/truncate.rs:232` (`+ 2`). Each must equal
the number of `ops.push` calls in a closure sixty to seventy lines below;
nothing enforces the relationship. `src/file_write.rs:321-322` explains its `3`
well. `src/truncate.rs:228-231` has a good comment, but it explains why the
*sum* is not a constant, not what the `2` is. `src/unlink.rs:298` has no comment
at all — two unnamed literals whose only cross-check is counting
`src/unlink.rs:314-338` by hand. `src/create.rs`'s `3` is explained only by the
stale half of H8c.

**M7 — three user-visible error strings lost their line continuations.**
`src/create.rs:244` (18 stray spaces), `src/inode_btree.rs:223` (14),
`src/write.rs:272` (22). Each renders with a gap mid-sentence, e.g. *"…sets bits
outside the permission mask, which&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;would
change the file's type"*. Every other multi-line `format!` in the crate uses `\`
plus aligned continuation correctly.

**M8 — `src/log_write.rs:350-440` `field_layout` applies two standards forty
lines apart.** `COMMON` (`:352-368`) and `V3_TAIL` (`:370-377`) carry a field
name on every tuple (`// di_magic`, `// di_nblocks`). The `times`, `counts` and
v2 tables (`:396-415`, `:420-432`) carry none: `&[(32, 8), (40, 8), (48, 8),
(144, 8)]` and `&[(32,4),(36,4),(40,4),(44,4),(48,4),(52,4),(76,4),(80,2)]`.
These are `ATIME`/`MTIME`/`CTIME`/`CRTIME` and the extent counts, all named in
`src/format/log_items.rs:669-690`. This is the merely-present half of the file's
literal cluster; the `:350-392` half is explained and fine.

**M9 — `src/log_write.rs:240` `for k in 0..blocks.min(64)`.** The `64` is named
in the reservoir as `rec_header::XLOG_CYCLE_DATA_ENTRIES`
(`src/format/log_items.rs:162-166`) with its derivation. Beyond being bare, the
`.min()` silently *drops* the excess. With the default 32 KiB iclog the cap
cannot be hit (`append_at` allows 63 payload blocks), but XFS permits `logbsize`
up to 256 KiB, where blocks 64+ would be left unstamped and their displaced
words unrecorded — a malformed record with no error. A clamp is standing in for
an assertion.

**M10 — `src/dir_write.rs:211-221`, `:264-273`, `:305-315`** — the `SfEntry`
mapping closure is byte-identical in all three, and the three enclosing
functions share the same shape (read `has_ftype`, map, optionally push, call
`encode_short_form`). One `fn sf_entries(parsed, exclude: Option<&[u8]>)`
collapses them.

**M11 — the data-entry size formula exists in four spellings.**
`src/dir.rs:552` (`align_up` + named offset), `src/dir_write.rs:59-60`
(`div_ceil`, literal `8 + 1`), `src/dir_block.rs:86-87` (`div_ceil`, literal,
ftype hardcoded as always present), and an unused `const fn` at
`src/format/dir.rs:714`. Two rounding idioms, two field spellings. This formula
decides where every directory entry ends.

**M12 — `src/capi.rs` FFI boilerplate, and the messages have already drifted.**
The `Err(e) => { record(&e); -1 }` epilogue appears at 16 sites (`:259, 480,
507, 544, 622, 629, 637, 668, 675, 707, 742, 815, 864, 934, 949, 956`); the
NULL-check + `borrow_str` + deref preamble at 5 (`:610, 656, 800, 846, 906`).
The epilogue is straightforwardly extractable — the file already proves helper
extraction works here (`guard`, `fill_attr`, `resolve_for_write`). The preamble
resists a plain function, but it has already drifted: *"fs or out is NULL"*
(`:406, 468, 498`), *"fs or buf is NULL"* (`:611, 801`), *"fs is NULL"* (`:531,
847, 907`), *"fs or buf is NULL, or bufsize is zero"* (`:657`).

**M13 — `src/capi.rs:945` `resolve_for_write` is not write-specific.** It is the
`lookup_path` → `read_inode_raw` → record-and-bail sequence, and
`fs_xfs_read_file` (`:619-632`) and `fs_xfs_readlink` (`:665-678`) each inline
it verbatim, 14 lines apiece. The name is the only reason the read paths do not
call it. Renaming to `resolve` and calling it removes 28 lines and two levels of
the deepest nesting site in the crate (`:694`, indent 29).

**M14 — `src/dir_block.rs:117-237` `build` (121 lines).** The longest function
in the directory files and the one most worth splitting: five phases already
marked with banner comments and genuinely independent — header (`:132-142`),
entries + index accumulation (`:144-177`), index write + free record
(`:179-210`), best-free (`:212-226`), tail (`:228-234`), each a self-contained
`&mut block` write. Unlike `read_short_form`, only ~7 of its 121 lines are error
construction.

**M15 — `src/create.rs:455-467`** — a six-element tuple destructured from a
three-arm match on `(Option, Option)`:

```rust
let (fork, dir_fields, dir_size, dir_blocks, dir_nextents, dir_format) =
    match (&short_form, &converted) {
        …
        (None, Some(c)) => (c.fork.clone(), XFS_ILOG_DEXT, c.size, 1, 1, Format::Extents),
        (None, None) => unreachable!("one of the two is always taken"),
    };
```

Positions 4 and 5 are `dir_blocks` and `dir_nextents`; the reader must count
commas back to the binding to learn that. The `unreachable!` exists only because
one binary choice is encoded as two `Option`s — `enum ParentUpdate {
Inline(Vec<u8>), Converted(Converted) }` would name all six fields and delete
the arm. `src/create.rs:179-188` `Converted` is already the right shape and is
the template.

**M16 — `src/capi.rs:205-216` `file_type_code` returns bare `1..7`**, duplicating
the eight named `FS_XFS_FT_*` values in `include/fs_xfs.h:39-48`. The comment
*"Numeric file type shared with the header"* asserts the coupling without
encoding it — a second divergent copy of the ABI.

**M17 — `src/write.rs:256-278` `set_attributes` binds a device it does not
use.** `let Some(device) = self.writable.as_ref() else { return
Err(Error::ReadOnly) };` duplicates the identical guard inside `update_inode`
(`src/write.rs:437-439`), which it then calls — and then needs `let _ = device;`
to silence the warning. `truncate` in the same file spells the same check as
`if self.writable.is_none()` (`src/write.rs:343-345`) and needs no discard.

**M18 — `Filesystem::truncate` and `Filesystem::truncate_to_zero` have opposite
durability properties and near-identical names.** `src/write.rs:342` is
non-journalled, writes directly to the device, and **leaves the blocks
allocated** (its own doc, `:327-331`: *"It does not reclaim the space"*).
`src/truncate.rs:78` is journalled, touches nothing on disk, and **frees the
blocks**. Nothing in either name signals which journals, and `truncate_to_zero`
reads as a special case of `truncate` when it is the more complete operation.

**M19 — `src/file_write.rs:87-94` shadows `crate::inode`.** A private `mod
inode` (a less-documented copy of `group_write::inode`, which the file already
imports six other items from at `:73`) is declared three lines after `use
crate::inode::Format` at `:74`. `inode::SIZE` and `crate::inode::Format` then
mean different modules within a few lines of each other.

**M20 — `src/file_write.rs:99-123` `filled_core` and `src/group_write.rs:179-206`
`emptied_core` are the same function.** Both copy `raw`, write `SIZE` and
`NBLOCKS`, run the identical `nrext64` feature-bit read, branch to `NEXTENTS64`
or `NEXTENTS`, and bump `CHANGECOUNT`. They differ in the values written and a
`v5` guard taken as a parameter by one. The explanatory comment (*"Where the
data-extent count lives depends on a feature bit in the inode itself…"*) is
copied word-for-word — and dropped entirely from the third copy at
`src/create.rs:196-198`.

---

### Low

**L1 — `src/superblock.rs`: geometry bounds are unnamed and one is duplicated.**
`512..=32768` for `sectsize` appears at both `:320` (in `parse`) and `:421` (in
`validate`); `512..=65536` at `:409`; `256..=2048` at `:433`. Each check
explains its *purpose* ("is not a sane power of two"); none names or sources its
bounds, and the duplicated pair can drift.

**L2 — `const OP_ALIGN: usize = 4;` declared four times with an identical doc
comment** — `src/create.rs:76`, `src/unlink.rs:69`, `src/dir_write.rs:48`,
`src/file_write.rs:85`. It is a log-format property and belongs in `log_write`
or the reservoir.

**L3 — `vec![0u8; 176]` in four test bodies** — `src/create.rs:665`, `:698`,
`src/unlink.rs:358`, `:392`. `inode::XFS_DINODE_V3_SIZE` names it one module
over.

**L4 — `src/buf_write.rs:257, 269, 271-272`: bytes-per-word `4` open-coded four
times**, while bits-per-word is named and documented (`const CHUNKS_PER_WORD:
usize = 32;`, `:70`). The `4` at `:257` is a structure stride and the `4` at
`:269` is a field width — different facts, identically spelled. The test at
`:359` asserts `"op_len == 20 + 4 * map_size"` with `BLF_HEADER_SIZE` in scope.

**L5 — `src/buf_write.rs:133-136` zeroes an already-zero vector**, with a comment
that makes it read as load-bearing. `BufferItem::new` initialises `dirty: vec![0;
words]` at `:116` and nothing marks anything between. Either drop it or make it
an assertion.

**L6 — `src/fs.rs:563-568` and `:615-620` give the same `.`/`..` justification
twice**, about forty lines apart, in near-identical wording. If one is edited the
other lies. (Also: `file_block += blocks_per_dir_block` at `:625` cannot advance
if `dirblocksize < blocksize`. That cannot happen in XFS, so it is noted rather
than raised.)

**L7 — the trailing-tag offset is a bare `- 2` three times** — `src/dir.rs:619`,
`:648`, `src/dir_block.rs:168` — while `format::dir::offsets::data_entry::tag()`
and `data_unused::tag()` exist unused (`src/format/dir.rs:708`, `:737`).
`dir_block.rs` is internally inconsistent: `:207` uses the named accessor,
`:168` hand-writes `len - 2`, eleven lines apart.

**L8 — `src/dir.rs:900, 913, 989`: bare `+ 2` and `* 2`** where
`format::dir::offsets::leaf_hdr::stale()`, `node_hdr::level()` and
`XFS_DIR2_BEST_SIZE` exist for exactly those.

**L9 — `src/dir_block.rs:173, 208, 229`: `expect("fits")`.** The same file's
`expect("a directory block is at most 64 KiB")` at `:167` and `:204` shows what
the message should say.

**L10 — `src/write.rs:527-549`: the test superblock builder ignores
`superblock::offsets`.** `b[4..8]`, `b[84..88]`, `b[120]` with trailing
name-comments, and — worst — `crc32c_with_zeroed_crc(&b, 224)` at `:547`, a bare
`224` with no comment one line above `b[224..228]`, where
`superblock::offsets::CRC` is available. Compare `src/group_write.rs:391`, which
correctly passes `btree::CRC`.

**L11 — `src/log.rs:136-137` and `:162-167`.** `SCAN_CHUNK: usize = 1 << 20`
carries a comment that restates its name; the interesting facts (why 1 MiB, and
that it is allocated in full even for a 64 KiB log, `:394`) go unsaid.
`header[..XLOG_REC_HEADER_SIZE.min(header.len())]` silently computes a wrong
checksum for a short header rather than refusing.

**L12 — `found: u64::MAX` as a "not applicable" sentinel**, at `src/dir.rs:501`,
`src/alloc_btree.rs:216`, `src/bmbt.rs:200`, `src/inode_btree.rs:283`, each with
a comment admitting the field is meaningless there. A
`BlockIdentityMismatch::Uuid` variant, or `found: Option<u64>`, would say it in
the type. As written, the error's `Display` prints an address that was never on
disk.

**L13 — `src/create.rs:532-533`: two unexplained `.clear()` calls.**
`inobt_raw.clear(); finobt_raw.clear();` after `changed_chunks` has finished with
them — the only reason those bindings are `mut` (`:406-407`). The identical code
in `src/unlink.rs:218-219` declares them non-`mut` and does not clear. Whichever
is right, the asymmetry between two otherwise-identical functions has no
explanation.

**L14 — `src/capi.rs:243, 249`: `borrow_str` returns ENOENT for a NULL pointer
and for invalid UTF-8**, where every other NULL check in the file returns EIO
(`:406, 468, 531, 611, 657, 801, 847, 907`). `errno_for`'s own doc (`:46-50`)
argues the mapping matters — *"a client distinguishes 'this file is not here'
from 'this volume is damaged' only by the errno"* — and a NULL argument is
neither.

**L15 — `src/extent.rs:86, 137`: bare `>> 63` and `<< 63`** for the unwritten
flag, surrounded by fully named bit constants (`STARTBLOCK_BITS_IN_HIGH_HALF`,
`BLOCKCOUNT_BITS`, `STARTOFF_MAX`) and a good comment about the field that spans
both halves. The one bit position in the record that is not named.

---

## Considered and not raised

| # | Item | Reason |
|---|---|---|
| 1 | *"`dir.rs` has nearly doubled since the last review"* | **False premise.** The prior review's "1,271 lines" was production-only, as its own preamble states. At commit `1c6a743`, the day the file was written, `#[cfg(test)]` began at line 1272 and the file was already **2,441 total lines**. It is now **2,402** — `git diff --stat 1c6a743 HEAD -- src/dir.rs` is 37 insertions, 76 deletions. The file has **shrunk by 39 lines** and gained one 12-line `match` (`ftype_to_raw`). |
| 2 | The prior review's H1: split `dir.rs` into a `dir/` module per storage form | **Less justified than before**, not more. Production logic is ~41% shared / 59% form-specific, and the forms are not cleanly separable — `parse_data_block` (`:764`) delegates into `parse_block_form`, and `parse_leaf`/`parse_node` both route through `da_hdr_size` + `da_counts`. A four-way split would put a 164-line shared `entry.rs` behind three thin files of 124/205/188 lines. M1 (deleting the duplicated `offsets` module) removes 128 lines mechanically and is a strictly better first move; it may make the split moot. |
| 3 | The prior review's diagnosis of `read_short_form` (`src/dir.rs:1095`, 137 lines) | **Diagnosis was wrong.** It attributed the length to "handling the 4-byte and 8-byte inode-number variants inline". That is three lines (`:1131-1133`) and the read is already delegated to `read_sf_ino` (`:1045`). **49 of the 137 lines (35%) are `Error::BadSuperblock(format!(…))` construction** across 11 error paths — the same ratio as `parse_entries` (33%), `parse_node` (35%), `parse_block_form` (37%) and `verify_v5_header` (35%). It is house style, not a defect in this function. If it is shortened, the lever is the error boilerplate. |
| 4 | `Superblock::parse` (115 lines), `Inode::parse` (108), `Ag::parse` (78), `Agi::parse` (62) | **Acceptable pattern.** Flat field-by-field mapping through named `offsets`, one abstraction level throughout. Splitting makes them harder to check against the format documentation, not easier. The prior review reached the same conclusion and it still holds. |
| 5 | `src/format/` — 3,104 lines, 165 of ~300 constants unreferenced outside the module | **Acceptable pattern**, and argued at length in `src/format/mod.rs:1-70`: an offset is only checkable against the specification when its neighbours are named. Several layouts here carry provenance that cost hours and cannot be looked up (the journal chapter of the published spec is the word `TODO:`). This is not dead code. H4 and M1 are about it not being *used*, which is the opposite complaint. |
| 6 | `ag.rs:148 check_identity` (6 params) / `ag.rs:395 verify_crc` (5) — the prior review's M3 `BlockContext` suggestion | **Below threshold.** Two call sites each (`:223`/`:230` and `:337`/`:344`). The extraction rule wants three. The grouping does recur in `dir.rs:478 verify_v5_header` (6 params), so if that one is touched the struct becomes worth it — noted at M25's location rather than raised separately. |
| 7 | `fs_xfs_set_attributes` (9 parameters, `src/capi.rs:894`) | **Acceptable as scoped.** `include/fs_xfs.h:260-263` declares the same nine, so this is the ABI as published, not an accident of the Rust side. A `const fs_xfs_attr_change_t *` would be better and would extend without breaking, but that is an ABI change, not a readability fix. (Unrelated and worth a glance: `ctime: None` is hardcoded at `:929` with no comment while the doc at `:878` says "timestamps".) |
| 8 | `parse_entries` "indent 21" and `rename_in_directory` "indent 24" | **False positive from the metric.** Both maxima land on wrapped `format!` string continuations and a flat `vec![Op { … }]` literal respectively. Control-flow nesting in `parse_entries` is `fn → while → if → if`, every inner `if` an immediately-returning guard; in `rename_in_directory` it never exceeds two. |
| 9 | `truncate_to_zero` (185 lines, `src/truncate.rs:78`) | **Leave it.** Best-structured of the three long write functions: thirteen blocks, and every block needing a why has one (`:113-115`, `:184-186`, `:201-203`, `:205-207`, `:228-231`). Single register, and the linear read genuinely is the transaction. At most extract `:116-135`. |

---

## Test and coverage baseline

| | value |
|---|---|
| Tests passing before | 313 |
| Tests failing before | 0 |
| Tests ignored | 12 (the `xfsprogs` shell-outs in `tests/oracle_mkfs.rs`, run via `cargo test -- --ignored`) |
| Suites | 27 (25 integration + unit + doc) |
| `cargo fmt --check` | clean |
| `cargo clippy --locked --all-targets -- -D warnings` | clean |
| Production lines reviewed | 18,229 across 29 files |
| Unit tests in `src/` | 236 across 19 files |

Nothing was changed, so there is no "after" column.

**Coverage notes that bear on any future refactor:**

- **Four modules have no in-file unit tests at all:** `src/capi.rs`,
  `src/dir_write.rs`, `src/fs.rs`, `src/truncate.rs`. `capi.rs` is well covered
  externally by `tests/capi.rs` (28 tests). `dir_write.rs`, `fs.rs` and
  `truncate.rs` are covered *only* by oracle tests.
- **The oracle tests skip silently when their fixtures are absent.** They
  `eprintln!` and `return`, which reports as a pass. `.vm-share/` is gitignored
  and holds 51 files on this machine, so everything ran here — but a green
  `cargo test` on a fresh clone proves much less than it appears to. Anything
  that touches `create`, `unlink`, `truncate`, `file_write` or `dir_write`
  should be verified with the fixtures present, and the fixture set confirmed
  before trusting the result.
- **H2 is completely uncovered.** `checkpointed` appears in no test file, and
  `tests/fs_refusals.rs` — the natural home for it — is entirely read-path. Any
  fix should land with a test that refuses an operation and then performs a
  legitimate one on the same handle.
- **H5's `bmbt` gap is uncovered.** `tests/alloc_btree_oracle.rs` and
  `tests/inode_btree_oracle.rs` exist; there is no `bmbt_oracle`, and no test
  feeds any of the three parsers a block read from the wrong address.

*Note: running `cargo test` boots the oracle VM. It was shut down after this
review.*

---

## What I would fix first

Ordered by value per unit of risk. The first three are the ones I would actually
do; everything after that is cleanup that can wait for a quiet afternoon.

**1. H2 — move `begin_checkpoint()?` below the guards in all six entry points.**
This is the only item on the list that is a live defect rather than a
readability cost, and it is a handful of lines. It needs one new test
(refuse-then-succeed on the same handle) which does not exist today, and that
test is worth having regardless. Do this one first because it is small, it is
provable, and it is currently invisible to the entire suite.

**2. H8 — delete or correct the four lying comments.** Zero behavioural risk,
and item (d) pays for itself immediately: once the doc is corrected,
`dir_has_ftype` is revealed as redundant and can go, taking one of the three
copies of `0x200` with it. Comments that assert the opposite of the code are the
cheapest thing on this list to fix and the most expensive to leave, because they
actively mislead the next person — including whoever implements items 3 and 4.

**3. H3, then H4, then M1 — one offset table per structure.** These are the same
job three times and should be done as one campaign, in that order (inode core is
the widest-spread and the most dangerous; the log header is the one the crate
explicitly forbids; `dir.rs`'s is the biggest single deletion). All three are
mechanical: delete the copy, `pub use` the canonical module, let the compiler
find the call sites. The replay oracles will catch a mistake. Doing this restores
the property the previous review recorded as holding, and it is the single
highest-value change on the list — it removes not four findings but the mechanism
that produced them.

**4. H6 and H7 — call `allocate_in_group`, and rename the two `emptied_core`s.**
H6 deletes ~78 lines and needs `tests/write_oracle.rs` and
`tests/file_write_replay_oracle.rs` green; H7 is a two-symbol rename to
`freed_core` / `truncated_core`. Both are contained.

**5. H1 — decide what `rename_in_directory` does about v4, and write it down.**
This needs a decision rather than a refactor: either add the v5 gate the other
four have, or add a comment saying why rename is safe without one. Either
outcome is fine; the current state, where the answer is unavailable, is not.

**6. H5 — reconcile the three btree parsers.** Left until after the offset
campaign because that campaign will change these files anyway. The readability
work (a shared `Node` and a shared header-verification helper) and the open
question (does `bmbt` skip the `bb_blkno` check on purpose?) should be settled
together, and the answer to the second determines the shape of the first.

**Then the Mediums**, of which M2–M6 are all downstream of the write-path
duplication and mostly dissolve once H6 is done. M7 (three mangled error
strings) is a two-minute fix worth doing whenever the files are open.

**The Lows can stay** unless the file is open for another reason. None of them
would repay a dedicated pass.

---

## What is good, and should be protected

Recording these because a refactor is the moment they get flattened by accident.

- **`src/endian.rs` is exemplary.** One definition of `be16`/`be32`/`be64`/`le32`
  for the whole crate, the byte-order rule stated once with the kernel function
  that justifies the checksum exception, the history of the three shipped bugs
  that motivated it, and a test asserting the big- and little-endian readers
  disagree on the same input so they can never quietly converge.
- **`src/format/log_items.rs` is the best-documented on-disk module in the
  family.** Every constant records how it was *pinned* — *"Pinned by chown 4321
  against 8765"*, *"Pinned by 3 against 402 after 400 creations"* — and there is
  a "Left open" section naming what is *not* known, including a recorded
  negative result (*"the AGFL never appeared as a buffer item"*) so it is not
  silently re-litigated. H4 is entirely about this module not being used.
- **Comments that explain rejected alternatives.** `src/dir.rs:271-292`
  `ftype_to_raw`: *"Written as a match rather than a cast because the enum's own
  discriminants are not these values, and a cast would compile, produce plausible
  bytes, and label every entry wrongly."* `src/dir_write.rs:241-252`, "Why not
  fitting is an answer rather than an error". These are the hardest comments to
  write and the most valuable.
- **Comments that explain non-obvious *omissions*.** The stale-CRC note appears
  six times (`src/create.rs:428-430`, `src/unlink.rs:252-253`,
  `src/truncate.rs:201-203`, `src/file_write.rs:272-274`,
  `src/group_write.rs:142`, `:335`), each pointing back to the canonical
  explanation in `restamp_crc` — which in turn cites its evidence: *"one run in
  619 of the 620 in the corpus"*.
- **Hazard lists at the top of the hard modules.** `src/buf_write.rs:30-46`
  ("Three things that are easy to get wrong", with the measurements that ruled
  out the alternatives), `src/dir_block.rs:30-51` ("Four things that are easy to
  get wrong", each with the *symptom* it produces), `src/log.rs:29-36` (why the
  whole-ring scan was chosen over the kernel's own approach, argued on risk).
- **ASCII operation-layout diagrams** beside the code that produces them —
  `src/file_write.rs:28-37`, `src/truncate.rs:11-20`, `src/buf_write.rs:17-22`.
- **Layered verification with distinguishing errors.** Magic, then identity
  (owner, UUID, block address), then checksum, with `ChecksumMismatch` and
  `BlockIdentityMismatch` as separate variants so a misdirected read is reported
  as a misdirected read. `src/dir.rs:34-37` explains why both are needed.
- **Error messages that teach.** `src/unlink.rs:166-170` ends with *"truncate it
  first"*; `src/create.rs:394-397` says why the refusal exists; every
  `UnsupportedFeature` names the shape it saw, why it is not handled, and often
  what to do instead.
- **Tests named as claims**, several with doc comments explaining why the case
  matters — `a_newer_ordinary_record_outranks_an_older_unmount`,
  `the_extent_count_follows_the_inodes_own_feature_bit`,
  `a_multi_operation_record_is_not_an_unmount` — plus paired negative controls
  (`src/log.rs:653-662`, *"so the check above cannot pass by refusing
  everything"*).
- **A test module that disclaims its own reach.** `src/dir.rs:1235-1246`: *"these
  fixtures are built in-process, so they prove the parser is self-consistent and
  nothing more… Correctness is established by `tests/dir_oracle.rs`."* That is
  the `AGENTS.md` rule enforced in the place it is easiest to forget.
- **`Kind` (`src/create.rs:84-137`)** — every file/directory divergence contained
  in one 50-line enum, one method per difference, each doc naming the consequence
  of getting it wrong. The right containment for a two-way variation, and the
  template `create` should follow elsewhere (see M15).
