# Human-code findings — status

Tracks every **High** and **Medium** finding from
[`human-code-report-2026-08-28.md`](human-code-report-2026-08-28.md). The report
predates the work; this is the current position. Updated 2026-08-30.

**43 findings** — 8 High, 20 Medium, 15 Low. This covers the 28 High and Medium.

| | High | Medium |
|---|---|---|
| Fixed | 3 | 0 |
| Left for a human decision | 2 | 8 |
| Fixable, not yet done | 3 | 12 |

---

## The one that could corrupt a filesystem

**H1 — `rename_in_directory` was the only journalled write with no v5 gate.**

Every other journalled entry point refuses a v4 filesystem by name — `create`,
`unlink`, `truncate`, `file_write` all say "…writes v5 metadata; a v4 filesystem
is not supported". Renaming did not, and it journals the same thing they do: v5
self-describing headers with CRCs and owner fields that **a v4 filesystem has
nowhere to put**.

Gated now, with the same message shape. `renaming_refuses_a_v4_filesystem` uses
`xfs-nocrc`, the only v4 fixture in the matrix, and pins its version so the test
cannot pass for the wrong reason if that fixture ever changes. Without the gate
it fails: renaming proceeds.

---

## High

### H2 — every write entry point burned the mount's one checkpoint before validating — **fixed earlier**

[#48](https://github.com/antimatter-studios/rust-fs-xfs/pull/48).

### H8 — four comments assert the opposite of what the code does — **fixed**

**(a)** The module's list of things `create` "refuses by name" included *"a
parent … with no room left in its inode for another entry"*. That case is
handled, by `convert_to_block_form` — it is the feature, not the refusal, and a
reader trusting the list concludes the conversion branch is unreachable.

**(b)** The same list promised a "root with no room" refusal. There isn't one in
`create`; the capacity check lives in `unlink`.

**(d)** The most consequential, because it told the reader to make a fix that
already exists. `dir_has_ftype` carried a doc saying `Superblock::has_ftype`
tests only the v5 bit and that "fixing it belongs in that module, not this
one" — and then repeated the `sb_features2` half itself. `has_ftype` **already
tests both**, so the duplicated clause was unreachable and the instruction was
stale. `dir_has_ftype` now delegates, and the two constants it kept for that
clause are gated to the tests that still use them.

**(c)** — the contradictory `mkdir` operation-count comments — is left: resolving
which of the two is right means re-deriving the transaction reservation, which
is the same work M6 asks for.

### H5 — one of three btree parsers omitted the block-address check — **fixed**

`alloc_btree::parse_block` and `inode_btree::parse_block` both verify a v5
block's self-recorded address. `bmbt::parse_block` did not: `grep -in 'blkno'
src/bmbt.rs` returned nothing at all, and the offsets module had no `BLKNO`
constant to return.

That left the block-map tree weakest against exactly the failure the check
exists for. A pointer corrupted into **another valid block of the same file**
passes the CRC — it is a real block — passes the owner check — same inode — and
passes the level check whenever the two sit at the same depth. Its recorded
address is the only field that separates them.

`AGENTS.md` states the rule the omission sat against: on v5, verify the CRC *and*
the self-describing identity fields.

`bb_blkno` is at 24 in the long form, by the same +8 shift that puts `UUID` at 40
instead of 32 — the long form carries 64-bit sibling pointers. It holds a
**basic-block address**, 512-byte units, not a filesystem block number, so the
conversion is now `alloc_btree::blkno_of_fsblock` and `expected_blkno` calls it:
two callers, one conversion, neither able to be right while the other is wrong.

Two tests. The first hands the walk a leaf that is correct in every other respect
— right inode, right level, valid checksum — read at the wrong block, then reads
the same block where it belongs to show it is not failing for some other reason.
The second pins the unit: a 4 KiB block is eight basic blocks, and stamping the
fsblock as-is is refused. That mistake would hide on 512-byte blocks and fire on
every real geometry.

**Every existing fixture had to start stamping its address**, which is its own
evidence: none of them recorded one, so none of them was a block the reader
should have accepted. Mutation-checked — removing the check fails both tests.

### H3, H4 — the inode-core offset table in seven places, the log record header in three

Both remain: the same `struct Node` and near-verbatim `parse_block`/`walk` in
three files, and the inode-core offsets spread across seven. H5 was the part of
that duplication with teeth — three parsers where one validated less — and the
rest is readability.

### H6, H7 — `write_into_empty_file` open-coding `allocate_in_group`; `emptied_core` naming two different live functions — **needs your decision**

H7 in particular: two live functions sharing a name and meaning different things
is a trap, and picking which keeps the name is a call about the module's
vocabulary.

---

## Medium

Twenty findings, none yet fixed. The shape of them:

**Duplication (M1–M5, M10, M11, M20)** — `dir.rs` duplicating `format/dir.rs`
across all 31 entry shapes; the write-path preamble four times; two 26-line
identical blocks in `create` and `unlink`; the free-space leaf update
triplicated; AG header addressing open-coded seven times; the `SfEntry` walk
three times; the data-entry size formula in four spellings; `filled_core` and
its twin.

**M2 and M5 first**, as the two whose copies are most likely to drift apart in
ways nothing catches.

**Correctness-adjacent (M6, M8, M9, M17, M18)** — item-operation tallies ending
in bare addends nothing checks; `field_layout` applying two standards forty
lines apart; a `.min(64)` whose 64 is named elsewhere; `set_attributes` binding
a device it does not use; **`truncate` and `truncate_to_zero` having opposite
argument orders**, which is the one most likely to be called wrongly.

**Naming and shape (M7, M12–M16, M19)** — three user-visible error strings that
lost their line continuations and now read as run-ons; FFI boilerplate whose
messages have already drifted; `resolve_for_write` not being write-specific;
`build` at 121 lines; a six-element tuple; `file_type_code` duplicating a
mapping; a private `mod inode` shadowing `crate::inode`.

**M7 is worth doing on its own** — it is user-visible and takes minutes.

---

## Verification

320 tests pass, up from 319. `chore lint` clean, and `src/dir.rs` now builds
with no warnings — removing the dead clause removed the reason for its imports.
