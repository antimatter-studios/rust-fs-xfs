//! `fs.xfs get` reports what xfsprogs reads from the same superblock.
//!
//! The images are the `cli` fixtures: made by `mkfs.xfs -L` and filled by
//! the kernel in the harness guest, one v5 and one v4. The tool's answer
//! is held to two independent readers, both run in the guest: `xfs_db`,
//! field by field from `sb 0`, and `xfs_info`, which is what a person
//! would ask. Neither shares a line of code with this crate, so a field
//! read from the wrong offset, in the wrong byte order or in the wrong
//! unit disagrees with them here rather than agreeing with a fixture of
//! our own making.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, oracle};

/// `name = value` from `xfs_db -r -c 'sb 0' -c 'print'`, as written.
fn db_field(dump: &str, name: &str) -> String {
    dump.lines()
        .find_map(|l| l.strip_prefix(&format!("{name} = ")))
        .unwrap_or_else(|| panic!("xfs_db printed no {name}:\n{dump}"))
        .trim()
        .to_string()
}

/// A number xfs_db printed, in decimal or in hex.
fn db_number(dump: &str, name: &str) -> u64 {
    let raw = db_field(dump, name);
    match raw.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => raw.parse(),
    }
    .unwrap_or_else(|e| panic!("xfs_db's {name} = {raw} is not a number: {e}"))
}

/// The label as xfs_db prints it: `"CLIV5\000\000..."`, padding and all.
fn db_label(dump: &str) -> String {
    let raw = db_field(dump, "fname");
    let inner = raw.trim_matches('"');
    inner.split("\\000").next().unwrap_or("").to_string()
}

/// `key=value` from `xfs_info`, the first time `key` appears after the
/// line starting with `section`.
fn info_field(info: &str, section: &str, key: &str) -> u64 {
    let from = info
        .find(section)
        .unwrap_or_else(|| panic!("xfs_info has no {section} section:\n{info}"));
    let rest = &info[from..];
    let at = rest
        .find(&format!("{key}="))
        .unwrap_or_else(|| panic!("xfs_info's {section} section has no {key}=:\n{info}"))
        + key.len()
        + 1;
    rest[at..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap_or_else(|e| panic!("xfs_info's {key} is not a number: {e}\n{info}"))
}

fn number(get: &str, key: &str) -> u64 {
    json_field(get, key)
        .parse()
        .unwrap_or_else(|e| panic!("get's {key} is not a number: {e}\n{get}"))
}

fn agrees(image: &str, label: &str, version: u64) {
    let get = stdout(&ok(tool("fs.xfs").args([image, "get"])));

    let sb = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "print", image])
        .output();
    assert!(
        sb.ok(),
        "xfs_db -r -c 'sb 0' -c print failed:\n{}",
        sb.stderr
    );
    let dump = &sb.stdout;

    assert_eq!(json_field(&get, "label"), label, "{get}");
    assert_eq!(db_label(dump), label, "xfs_db's fname:\n{dump}");
    assert_eq!(json_field(&get, "uuid"), db_field(dump, "uuid"), "uuid");
    assert_eq!(number(&get, "block_size"), db_number(dump, "blocksize"));
    assert_eq!(number(&get, "total_blocks"), db_number(dump, "dblocks"));
    assert_eq!(number(&get, "free_blocks"), db_number(dump, "fdblocks"));
    assert_eq!(number(&get, "ag_count"), db_number(dump, "agcount"));
    assert_eq!(number(&get, "ag_blocks"), db_number(dump, "agblocks"));
    assert_eq!(number(&get, "inode_count"), db_number(dump, "icount"));
    assert_eq!(number(&get, "free_inodes"), db_number(dump, "ifree"));
    assert_eq!(number(&get, "sector_size"), db_number(dump, "sectsize"));
    assert_eq!(number(&get, "inode_size"), db_number(dump, "inodesize"));
    assert_eq!(number(&get, "root_inode"), db_number(dump, "rootino"));
    assert_eq!(number(&get, "log_blocks"), db_number(dump, "logblocks"));
    assert_eq!(number(&get, "versionnum"), db_number(dump, "versionnum"));
    assert_eq!(number(&get, "version"), version);
    assert_eq!(
        number(&get, "total_bytes"),
        db_number(dump, "dblocks") * db_number(dump, "blocksize")
    );
    assert_eq!(
        number(&get, "free_bytes"),
        db_number(dump, "fdblocks") * db_number(dump, "blocksize")
    );

    let info = oracle("xfs_info").arg(image).output();
    assert!(info.ok(), "xfs_info failed:\n{}", info.stderr);
    let info = &info.stdout;
    assert_eq!(
        number(&get, "block_size"),
        info_field(info, "data", "bsize")
    );
    assert_eq!(
        number(&get, "total_blocks"),
        info_field(info, "data", "blocks")
    );
    assert_eq!(
        number(&get, "ag_count"),
        info_field(info, "meta-data", "agcount")
    );
    assert_eq!(
        number(&get, "ag_blocks"),
        info_field(info, "meta-data", "agsize")
    );
    assert_eq!(
        number(&get, "inode_size"),
        info_field(info, "meta-data", "isize")
    );
    assert_eq!(
        number(&get, "sector_size"),
        info_field(info, "meta-data", "sectsz")
    );
    assert_eq!(
        number(&get, "log_blocks"),
        info_field(info, "log", "blocks")
    );
    assert_eq!(
        info_field(info, "meta-data", "crc"),
        u64::from(version == 5),
        "xfs_info's crc= and get's version disagree"
    );
}

#[test]
fn get_agrees_with_xfs_db_and_xfs_info_on_a_v5_image() {
    let image = fixture("xfscli-v5.img");
    agrees(image.to_str().unwrap(), "CLIV5", 5);
}

#[test]
fn get_agrees_with_xfs_db_and_xfs_info_on_a_v4_image() {
    let image = fixture("xfscli-v4.img");
    agrees(image.to_str().unwrap(), "CLIV4", 4);
}
