//! `include/fs_xfs.h` declares every function the library exports, and
//! nothing the library does not (#291).
//!
//! The header is hand-written and the entry points live in Rust, so
//! nothing but this connects them. An entry point missing from the header
//! is one a C caller cannot reach without declaring it by hand, which is
//! how a declaration comes to disagree with the function it names. A
//! declaration with no function behind it compiles and fails at link.
//!
//! # Why this scans instead of parsing
//!
//! As in `header_sentinels_match_the_abi.rs`: a C header is not a format
//! with a parser here, and the Rust side is read the same way, as lines.
//! What a scan could get wrong is handled explicitly:
//!
//! - **A name in a comment is not a declaration.** `/* ... */` blocks are
//!   skipped, and the header names its functions in prose all the time.
//! - **A scan that finds nothing agrees with everything.** Both sides are
//!   required to be non-empty, and the mutations at the bottom prove a
//!   removed declaration and a removed export are each reported.

use std::collections::BTreeSet;
use std::path::PathBuf;

fn read(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The header's lines with `/* ... */` comments removed, including one
/// that opens and closes on a line with code before it.
fn code_lines(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        let mut kept = String::new();
        let mut rest = line;
        loop {
            if in_block {
                match rest.find("*/") {
                    Some(end) => {
                        rest = &rest[end + 2..];
                        in_block = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("/*") {
                    Some(start) => {
                        kept.push_str(&rest[..start]);
                        rest = &rest[start + 2..];
                        in_block = true;
                    }
                    None => {
                        kept.push_str(rest);
                        break;
                    }
                }
            }
        }
        out.push(kept);
    }
    out
}

/// Every `fs_xfs_*` name the header declares as a function: the name
/// directly followed by `(`. A typedef'd pointer, `(*fs_xfs_read_fn)`, is
/// followed by `)` and is not one.
fn declared(header: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for line in code_lines(header) {
        let mut rest = line.as_str();
        while let Some(at) = rest.find("fs_xfs_") {
            let tail = &rest[at..];
            let len = tail
                .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                .unwrap_or(tail.len());
            let preceded_by_word = rest[..at]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
            if !preceded_by_word && tail[len..].trim_start().starts_with('(') {
                names.insert(tail[..len].to_string());
            }
            rest = &tail[len..];
        }
    }
    names
}

/// Every function `src/capi.rs` exports: the `fn` the first code line
/// after a `#[no_mangle]` names. Doc comments and other attributes
/// between the two are passed over.
fn exported(source: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut pending = false;
    for line in source.lines() {
        let t = line.trim();
        if t == "#[no_mangle]" {
            pending = true;
            continue;
        }
        if !pending || t.starts_with("//") || t.starts_with("#[") {
            continue;
        }
        let name = t
            .split_once("fn ")
            .map(|(_, after)| after.split(['(', '<']).next().unwrap_or("").trim())
            .unwrap_or_else(|| panic!("#[no_mangle] is followed by {t:?}, which is not a fn"));
        names.insert(name.to_string());
        pending = false;
    }
    names
}

/// What differs between the two sets, as a sentence a failure can print.
fn disagreement(header: &BTreeSet<String>, library: &BTreeSet<String>) -> Option<String> {
    let undeclared: Vec<_> = library.difference(header).collect();
    let unexported: Vec<_> = header.difference(library).collect();
    if undeclared.is_empty() && unexported.is_empty() {
        return None;
    }
    Some(format!(
        "exported but not declared in include/fs_xfs.h: {undeclared:?}; declared but not \
         exported by src/capi.rs: {unexported:?}"
    ))
}

#[test]
fn the_header_declares_exactly_the_functions_the_library_exports() {
    let header = declared(&read("include/fs_xfs.h"));
    let library = exported(&read("src/capi.rs"));
    assert!(
        !header.is_empty() && !library.is_empty(),
        "a scan found nothing (header: {header:?}, library: {library:?}), and nothing \
         agrees with nothing"
    );
    if let Some(why) = disagreement(&header, &library) {
        panic!("{why}");
    }
}

/// The realtime mounts are part of that set by name: a caller with a
/// realtime volume has no other way to read its files (#291).
#[test]
fn the_realtime_mounts_are_declared_and_exported() {
    let header = declared(&read("include/fs_xfs.h"));
    let library = exported(&read("src/capi.rs"));
    for name in [
        "fs_xfs_mount_with_realtime",
        "fs_xfs_mount_with_realtime_callbacks",
    ] {
        assert!(
            header.contains(name),
            "include/fs_xfs.h does not declare {name}"
        );
        assert!(library.contains(name), "src/capi.rs does not export {name}");
    }
}

/// A gate that cannot fail is no gate: removing one declaration, or
/// hiding it in a comment, is reported.
#[test]
fn a_missing_or_commented_out_declaration_is_reported() {
    let text = read("include/fs_xfs.h");
    let library = exported(&read("src/capi.rs"));
    let victim = "fs_xfs_umount(";
    assert!(text.contains(victim), "the mutation's target moved");

    let removed = text.replace("void fs_xfs_umount(fs_xfs_fs_t *fs);", "");
    assert_ne!(removed, text, "the removal mutation changed nothing");
    let why = disagreement(&declared(&removed), &library).expect("a removed declaration");
    assert!(why.contains("fs_xfs_umount"), "{why}");

    let hidden = text.replace(
        "void fs_xfs_umount(fs_xfs_fs_t *fs);",
        "/* void fs_xfs_umount(fs_xfs_fs_t *fs); */",
    );
    let why = disagreement(&declared(&hidden), &library).expect("a commented-out declaration");
    assert!(why.contains("fs_xfs_umount"), "{why}");
}

/// And the other direction: an export the header does not know about.
#[test]
fn an_export_the_header_lacks_is_reported() {
    let header = declared(&read("include/fs_xfs.h"));
    let source = format!(
        "{}\n#[no_mangle]\npub extern \"C\" fn fs_xfs_not_in_the_header() {{}}\n",
        read("src/capi.rs")
    );
    let why = disagreement(&header, &exported(&source)).expect("an undeclared export");
    assert!(why.contains("fs_xfs_not_in_the_header"), "{why}");
}
