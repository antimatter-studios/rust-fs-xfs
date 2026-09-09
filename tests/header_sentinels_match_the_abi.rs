//! `include/fs_xfs.h`'s sentinels are the ones the ABI actually uses.
//!
//! The header is hand-written and the constants live in Rust, so
//! nothing but this connects them. A header that disagrees with the
//! library is worse than no header: the C caller compiles, links,
//! passes what it was told to pass, and gets the other behaviour.
//!
//! This matters more than usual for `FS_XFS_LEAVE_TIME`, which exists
//! precisely BECAUSE one sentinel was being used for two domains. Its
//! whole value is that a timestamp's "leave alone" is outside the range
//! of real dates, and that property lives in a number written in two
//! files.
//!
//! # Why this scans instead of parsing
//!
//! The standing rule is that a guard over a structured file parses it.
//! A C header is not one of the formats that has a parser here, so the
//! rule has no referent: there is nothing to route this through. That
//! is an exemption for the FORMAT, not a licence, so the two things a
//! scan gets wrong are handled explicitly rather than hoped about.
//!
//! WHAT A LINE SCAN CANNOT SEE HERE, and what is done about it:
//!
//! - **A `#define` inside a comment is not a definition.** Both
//!   sentinels are named in the prose above them, and this file's own
//!   family carries the scar: an unanchored pattern in `ci_profile.rs`
//!   matched the comment naming the construct it was checking for, and
//!   passed against a file that no longer contained the thing. So
//!   `/* ... */` blocks are tracked and skipped.
//! - **A name defined twice makes a blind match test the wrong one.**
//!   Uniqueness is asserted before content, in its own test, so a
//!   mutation cannot edit an occurrence the guard never reads.
//!
//! What it still cannot see, stated so nobody assumes otherwise: the
//! preprocessor. A sentinel defined behind an `#if`, or built from
//! another macro, would not be understood. Neither is true here, and if
//! one becomes true this guard must be told rather than trusted.

use std::path::PathBuf;

/// Each sentinel, the value the ABI uses, and how the header must spell
/// it. Both halves are asserted: the Rust constant against the number,
/// and the header against the spelling.
const SENTINELS: &[(&str, i64, &str)] = &[
    ("FS_XFS_LEAVE", fs_xfs::capi::FS_XFS_LEAVE, "((int64_t)-1)"),
    (
        "FS_XFS_LEAVE_TIME",
        fs_xfs::capi::FS_XFS_LEAVE_TIME,
        "((int64_t)INT64_MIN)",
    ),
];

fn header() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("include")
        .join("fs_xfs.h");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The header's lines with `/* ... */` blocks removed.
///
/// Crude, and in the safe direction: a definition mistaken for a
/// comment makes a sentinel look ABSENT, which fails loudly, where a
/// comment mistaken for a definition makes an absent one look present,
/// which is the failure this family has actually suffered.
fn code_lines(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let t = line.trim();
        if in_block {
            if t.contains("*/") {
                in_block = false;
            }
            continue;
        }
        if t.starts_with("/*") && !t.contains("*/") {
            in_block = true;
            continue;
        }
        if t.starts_with("/*") || t.starts_with("//") {
            continue;
        }
        out.push(line);
    }
    out
}

fn is_define_of(line: &str, name: &str) -> bool {
    let t = line.trim();
    t.starts_with("#define") && t.split_whitespace().nth(1) == Some(name)
}

#[test]
fn the_header_defines_each_sentinel_exactly_once() {
    let text = header();
    for (name, _, _) in SENTINELS {
        let count = code_lines(&text)
            .into_iter()
            .filter(|l| is_define_of(l, name))
            .count();
        assert_eq!(
            count, 1,
            "expected exactly one #define of {name} in include/fs_xfs.h, found {count}"
        );
    }
}

#[test]
fn the_header_spells_each_sentinel_the_way_the_abi_means_it() {
    let text = header();
    let lines = code_lines(&text);
    for (name, value, spelling) in SENTINELS {
        let mut found = lines.iter().filter(|l| is_define_of(l, name));
        let line = found
            .next()
            .unwrap_or_else(|| panic!("no #define of {name} outside a comment"));
        assert!(
            found.next().is_none(),
            "{name} is defined more than once; this assertion would be testing \
             whichever came first, which is not necessarily the one that compiles"
        );
        let body = line
            .trim()
            .splitn(3, char::is_whitespace)
            .nth(2)
            .unwrap_or("");
        assert_eq!(
            body.trim(),
            *spelling,
            "include/fs_xfs.h defines {name} as {body:?}; the library uses {value}, \
             which this file spells {spelling:?}. A C caller believes the header."
        );
    }
}

/// The Rust half of the same pair. Without this the table above could be
/// changed to match a drifted header and everything would go green.
#[test]
fn the_library_uses_the_values_the_table_names() {
    assert_eq!(fs_xfs::capi::FS_XFS_LEAVE, -1);
    assert_eq!(fs_xfs::capi::FS_XFS_LEAVE_TIME, i64::MIN);
}

/// AND THEY MUST DIFFER. Sharing one sentinel across a domain where
/// negatives are impossible and one where they are ordinary dates is
/// the defect this pair was split to fix.
#[test]
fn the_two_sentinels_are_not_the_same_value() {
    assert_ne!(fs_xfs::capi::FS_XFS_LEAVE, fs_xfs::capi::FS_XFS_LEAVE_TIME);
}

/// A `#define` INSIDE A COMMENT IS NOT A DEFINITION, and this is the
/// arm that proves the scan knows it. Deleting the real definition
/// while leaving one commented out must fail -- otherwise the guard
/// passes against a header that no longer defines the constant, which
/// is precisely how an unanchored pattern in this repository's
/// `ci_profile.rs` came to pass against a file that had lost the thing
/// it was checking for.
#[test]
fn a_define_inside_a_comment_does_not_count() {
    let commented = "\
#ifndef FS_XFS_H
#define FS_XFS_H
/*
 * Historical note, not a definition:
#define FS_XFS_LEAVE_TIME ((int64_t)INT64_MIN)
 */
#define FS_XFS_LEAVE ((int64_t)-1)
#endif
";
    let lines = code_lines(commented);
    assert!(
        lines.iter().any(|l| is_define_of(l, "FS_XFS_LEAVE")),
        "the real definition must still be seen"
    );
    assert!(
        !lines.iter().any(|l| is_define_of(l, "FS_XFS_LEAVE_TIME")),
        "a #define inside a block comment is prose, and counting it would let a \
         header that dropped the constant keep passing"
    );
}
