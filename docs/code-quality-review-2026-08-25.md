# Code quality review — 2026-08-25

**Scope:** `src/`, 4,993 production lines across 12 files (test modules excluded from
every count below).
**Findings:** 1 high, 2 medium, 2 low. No fixes applied — this is a read of the code
as it stands.

This crate is in good condition. It has no duplication, no unnamed offsets, and a
module layout that follows the on-disk format. The findings are about a handful of
long functions and one oversized file, not about structural problems.

Some of that is age — this is one of the newer crates — but a good deal of it is
deliberate: an earlier review of this crate centralised the byte-order helpers and
named all 83 of its offsets, and both decisions have held.

---

## H1 — `dir.rs` is 1,271 lines, a quarter of the crate

**`src/dir.rs`**

XFS directories have four storage forms — short form, block, leaf and node — and this
file implements all of them, plus the shared entry parsing, plus the checksum and
identity verification each form needs.

The forms genuinely share machinery, so one file is defensible. What makes it worth
flagging is that the file is now larger than the entire `rust-fs-squashfs` crate, and
the four forms are read by different people at different times: someone debugging a
short-form directory has no use for the leaf index.

`read_short_form` at 137 lines is the longest function in the crate.

**Shape of the fix.** A `dir/` module with one file per storage form and a shared
`entry.rs`, or — if that seems heavy — leave the file and split `read_short_form`,
whose length comes from handling the 4-byte and 8-byte inode-number variants inline
rather than from doing anything complicated.

---

## M2 — 11 functions of 60 lines or more

**`src/dir.rs:1134 read_short_form` (137), `src/superblock.rs:283 parse` (115),
`src/inode.rs:279 parse` (108), `src/bmbt.rs:157 parse_block` (83),
`src/dir.rs:632 parse_entries` (80), `src/ag.rs:203 parse` (78)**

Four of the six are `parse` functions, and those are the acceptable case: a flat
field-by-field mapping from bytes to a struct is at one level of abstraction the whole
way down, and splitting it makes it harder to check against the format documentation,
not easier. `Superblock::parse` has already had its checksum verification and feature
gating extracted for exactly this reason, and what remains is the flat part.

The two worth revisiting are `read_short_form` (see H1) and `parse_entries`, where the
per-entry decoding and the file-type-byte handling are separable.

**Recommendation:** treat the `parse` functions as fine, and do not let a line-count
target push them apart.

---

## M3 — Six functions take five or more parameters

**`src/ag.rs:check_identity` (6), `src/ag.rs:verify_crc` (5),
`src/bmbt.rs:parse_block` (5), and three others**

`check_identity` and `verify_crc` both take some combination of *what structure this
is*, *where it was read from*, *who owns it*, and *the superblock*. That group recurs,
and it is a `BlockContext { what, daddr, owner }` waiting to be named — which would
also stop the two functions' parameter orders being independently memorable.

Small and safe.

---

## L4 — 11 lines indented 24 columns or deeper

**crate-wide**

The lowest count of any large crate in the family (`rust-fs-ext4` has 271). Noted only
to record that it is not a problem here.

---

## L5 — One `#[allow(dead_code)]`, and it is explained

**`src/log.rs`**

The single suppression in the crate carries its reasoning: the offset table names the
fields between the ones actually read, because an offset can only be checked against
the specification when its neighbours are there to count off against. That is the right
pattern; it is recorded here as the example the other crates should follow rather than
as a finding.

---

## What is good

- **No duplication at all.** Zero repeated eight-line blocks across 4,993 lines.
- **No unnamed multi-digit offsets.** Every structure has a documented `offsets`
  module. This was a deliberate change and it has not regressed.
- **One `endian` module.** `be16` / `be32` / `be64` / `le32` are defined once, with the
  format's rule — big-endian throughout except checksums — stated in one place rather
  than in each parser. A test asserts the big-endian and little-endian readers disagree
  on the same input, so the two can never quietly converge.
- **Verification is layered.** Structures check magic, then identity (owner, UUID,
  block address), then checksum, and the errors distinguish the three. A misdirected
  read is reported as a misdirected read, not as corruption.
- **Cross-validation against `xfs_db` and against filesystems the kernel wrote**,
  with fixtures that assert they still contain the features they are meant to cover.
- **`clippy -D warnings` and `rustfmt` are clean**, and CI enforces both.

## Suggested order

M3 first — it is an afternoon's work and removes a recurring parameter group. H1 next,
and the lighter version of it (splitting `read_short_form`) may well be enough; the
full `dir/` split is worth doing only if the file keeps growing.

Nothing here is urgent.
