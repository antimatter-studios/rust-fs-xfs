//! Declared operation outcomes; Linux evidence must confirm each successful write.
#![allow(dead_code)]

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expected {
    Write,
    Refuse(&'static str),
    Absent,
}

use Expected::{Absent as A, Refuse as R, Write as W};

// Columns: create file, create directory, rename, unlink, truncate zero,
// write empty, setattr, truncate shared, truncate partly shared, conversion,
// create in a later AG. Each row explicitly fixes every operation's outcome.
const PLAIN: [Expected; 11] = [W, W, W, W, W, W, W, A, A, W, W];
const SHARED: [Expected; 11] = [W, W, W, W, W, W, W, W, W, W, W];
const LEGACY: [Expected; 11] = [
    R("v4"),
    R("v4"),
    R("v4"),
    R("v4"),
    R("v4"),
    R("v4"),
    R("has no CRC"),
    A,
    A,
    R("v4"),
    R("v4"),
];
const QUOTA: [Expected; 11] = SHARED;
// In-place attributes work; every checkpoint writer rejects the 64 KiB
// log record headers produced by this stripe geometry.
const STRIPE: [Expected; 11] = [
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
    W,
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
    R("multi-block record header"),
];

pub const ROWS: &[(&str, [Expected; 11])] = &[
    ("v4", LEGACY),
    ("base", PLAIN),
    ("finobt", PLAIN),
    ("finobt-inobtcount", PLAIN),
    ("reflink", SHARED),
    ("reflink-finobt", SHARED),
    ("reflink-finobt-inobtcount", SHARED),
    ("rmapbt", PLAIN),
    ("rmapbt-finobt", PLAIN),
    ("rmapbt-finobt-inobtcount", PLAIN),
    ("rmapbt-reflink-nofinobt", SHARED),
    ("rmapbt-reflink", SHARED),
    ("everything", SHARED),
    ("bigtime0", SHARED),
    ("nrext64", SHARED),
    ("nrext64-bigtime0", SHARED),
    ("sparse", SHARED),
    ("nosparse", SHARED),
    ("b1k", SHARED),
    ("b2k", SHARED),
    ("i1k", SHARED),
    ("dirblock8k", SHARED),
    ("ci", SHARED),
    ("fullinodes", SHARED),
    ("meta_uuid", SHARED),
    ("quota", QUOTA),
    ("stripe", STRIPE),
    ("sector4k", SHARED),
];

pub const OPS: &[&str] = &[
    "create_file",
    "create_directory",
    "rename_in_directory",
    "unlink_file",
    "truncate_to_zero",
    "write_into_empty_file",
    "set_attributes",
    "truncate_shared",
    "truncate_partly_shared",
    "convert_directory",
    "create_in_later_group",
];

pub fn expected(combo: &str, op: &str) -> Expected {
    let row = ROWS
        .iter()
        .find(|(name, _)| *name == combo)
        .expect("unknown matrix row");
    let column = OPS
        .iter()
        .position(|name| *name == op)
        .expect("unknown matrix operation");
    row.1[column]
}

pub fn require_expected(combo: &str, op: &str, refusal: Option<&str>) {
    match (expected(combo, op), refusal) {
        (W, None) => (),
        (R(reason), Some(actual)) if actual.contains(reason) => (),
        (A, Some(actual))
            if actual.starts_with("not applicable: no shared")
                || actual.starts_with("not applicable: no partly shared") => {}
        (wanted, actual) => panic!("{combo}/{op}: expected {wanted:?}, actual refusal {actual:?}"),
    }
}
