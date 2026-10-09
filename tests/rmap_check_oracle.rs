//! `fsck.xfs` checks the reverse-mapping btree against the owners it
//! walks, and agrees with `xfs_repair -n` about what is wrong (#380).
//!
//! The volume is the `reflink` fixture, made by `mkfs.xfs -m
//! reflink=1,rmapbt=1` and filled by the kernel. That it is clean, to the
//! reference and to `fsck.xfs`, is `tests/cli_fsck_oracle.rs`. Here one
//! reverse-mapping record of a file's data is damaged at a time with
//! `xfs_db -x`, which recomputes the block's checksum, so each case is the
//! damage it names and not a checksum failure:
//!
//! - **wrong owner**: the record names an inode that does not own the
//!   blocks;
//! - **missing**: the record is shortened by a block, which the file still
//!   owns;
//! - **stale**: the record is moved onto free space.
//!
//! `xfs_repair -n` must find each one, or the case is not damage and the
//! test says so; then `fsck.xfs` must exit 4 and report the code.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, oracle, scratch};

const SUITE: &str = "rmap_check_oracle";

/// One leaf record of AG 0's reverse-mapping btree, as `xfs_db` prints it.
#[derive(Debug, Clone, Copy)]
struct Record {
    index: usize,
    startblock: u64,
    blockcount: u64,
    owner: i64,
}

/// Every record of AG 0's reverse-mapping btree root, which on this
/// fixture is a leaf.
fn records(image: &str) -> Vec<Record> {
    let level = oracle("xfs_db")
        .args(["-r", "-c", "agf 0", "-c", "addr rmaproot", "-c", "p level"])
        .arg(image)
        .output();
    assert!(
        level.stdout.trim().ends_with("= 0"),
        "AG 0's reverse-mapping root is not a leaf, so these cases need another \
         fixture:\n{}",
        level.stdout
    );
    let shown = oracle("xfs_db")
        .args(["-r", "-c", "agf 0", "-c", "addr rmaproot", "-c", "p recs"])
        .arg(image)
        .output();
    // `recs[1-N] = [startblock,blockcount,owner,offset,...] 1:[0,4,-3,0,0,0,0] 2:[...]`
    let mut out = Vec::new();
    for item in shown.stdout.split_whitespace() {
        let Some((n, fields)) = item.split_once(":[") else {
            continue;
        };
        let fields: Vec<&str> = fields.trim_end_matches(']').split(',').collect();
        if let (Ok(index), [start, count, owner, ..]) = (n.parse::<usize>(), fields.as_slice()) {
            out.push(Record {
                index,
                startblock: start.parse().expect("startblock"),
                blockcount: count.parse().expect("blockcount"),
                owner: owner.parse().expect("owner"),
            });
        }
    }
    assert!(
        !out.is_empty(),
        "xfs_db printed no records:\n{}",
        shown.stdout
    );
    out
}

/// A copy of the fixture with `commands` applied to AG 0's rmap root.
fn damaged(name: &str, commands: &[String]) -> scratch::Volume {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-reflink.img"),
        &format!("{}-{name}.img", std::process::id()),
    );
    let mut db = oracle("xfs_db").args(["-x", "-c", "agf 0", "-c", "addr rmaproot"]);
    for c in commands {
        db = db.args(["-c", c]);
    }
    let edit = db.arg(copy.path()).output();
    assert!(
        edit.ok(),
        "{name}: xfs_db could not make the damage:\n{}{}",
        edit.stdout,
        edit.stderr
    );
    copy
}

fn assert_found(name: &str, volume: &scratch::Volume, code: &str) {
    let image = volume.path().to_str().unwrap();
    let repair = oracle("xfs_repair").args(["-n", image]).output();
    let report = repair.repair_report();
    assert!(
        !common::repair::was_blind(&report),
        "{name}: xfs_repair -n declined to look:\n{report}"
    );
    assert!(
        !repair.ok(),
        "{name}: xfs_repair -n calls this clean, so it is not damage:\n{report}"
    );
    let out = tool("fsck.xfs")
        .args(["--text", image])
        .output()
        .expect("spawn fsck.xfs");
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert_eq!(out.status.code(), Some(4), "{name}: fsck.xfs exit\n{text}");
    assert!(
        text.contains(&format!(": {code}: ")),
        "{name}: fsck.xfs did not report {code}; xfs_repair -n said:\n{report}\nfsck.xfs said:\n{text}"
    );
}

/// The file record to damage: a data extent of more than one block.
fn file_record(image: &str) -> Record {
    records(image)
        .into_iter()
        .find(|r| r.owner > 0 && r.blockcount > 1)
        .expect("a multi-block file record in AG 0")
}

#[test]
fn a_record_naming_the_wrong_owner_is_found() {
    let source = fixture("xfs-reflink.img");
    let r = file_record(source.to_str().unwrap());
    let volume = damaged(
        "owner",
        &[format!("write -d recs[{}].owner {}", r.index, r.owner + 7)],
    );
    assert_found("wrong owner", &volume, "rmap.owner");
}

#[test]
fn a_record_that_stops_short_of_its_extent_is_found() {
    let source = fixture("xfs-reflink.img");
    let r = file_record(source.to_str().unwrap());
    let volume = damaged(
        "short",
        &[format!(
            "write -d recs[{}].blockcount {}",
            r.index,
            r.blockcount - 1
        )],
    );
    assert_found("record a block short", &volume, "rmap.missing");
}

#[test]
fn a_record_moved_onto_free_space_is_found() {
    let source = fixture("xfs-reflink.img");
    let image = source.to_str().unwrap();
    // The last file record, moved onto the last free extent, which lies
    // past every allocation, so the records stay in order and only the
    // owner of the blocks is wrong.
    let r = records(image)
        .into_iter()
        .rfind(|r| r.owner > 0)
        .expect("a file record in AG 0");
    let numrecs = db_number(image, &["agf 0", "addr bnoroot", "p numrecs"]);
    let start = db_number(
        image,
        &[
            "agf 0",
            "addr bnoroot",
            &format!("p recs[{numrecs}].startblock"),
        ],
    );
    assert!(
        start > r.startblock,
        "the last free extent is not past the last file record"
    );
    let volume = damaged(
        "stale",
        &[
            format!("write -d recs[{}].startblock {start}", r.index),
            format!("write -d recs[{}].blockcount 1", r.index),
        ],
    );
    assert_found("record moved onto free space", &volume, "rmap.stale");
}

/// The number `xfs_db -r` prints for the last of `commands`.
fn db_number(image: &str, commands: &[&str]) -> u64 {
    let mut db = oracle("xfs_db").arg("-r");
    for c in commands {
        db = db.args(["-c", c]);
    }
    let shown = db.arg(image).output();
    shown
        .stdout
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "xfs_db printed no number for {commands:?}:\n{}",
                shown.stdout
            )
        })
}
