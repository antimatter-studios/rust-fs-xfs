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

### H3, H4, H5 — the inode-core offset table in seven places, the log record header in three, btree node parsing triplicated — **fixable, not yet done**

H5 is the one to do first and the reason is in the finding: **one of the three
copies silently omits an identity check**. Three parsers of the same structure
where one validates less than the others is a bug with a delay on it.

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
