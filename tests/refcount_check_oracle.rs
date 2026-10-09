//! `fsck.xfs` checks the refcount btree against the file mappings it
//! walks, and agrees with `xfs_repair -n` about what is wrong (#379).
//!
//! The volume is made in the harness guest with `mkfs.xfs -m reflink=1`,
//! and the kernel shares one file's blocks three ways with
//! `cp --reflink=always`, so AG 0's refcount btree holds a record with a
//! count of 3. That volume must be clean to the reference and to
//! `fsck.xfs`. Then one refcount record is damaged at a time with
//! `xfs_db -x`, which recomputes the block's checksum:
//!
//! - **wrong count**: the shared extent's count is lowered to 2, while
//!   three files still map it;
//! - **stale**: the record is moved onto free space, which nothing maps.
//!
//! `xfs_repair -n` must find each one, or the case is not damage and the
//! test says so; then `fsck.xfs` must exit 4 and report the code.

mod cli_support;
mod common;

use cli_support::*;
use common::{kernel_run, oracle, repair, scratch};

const SUITE: &str = "refcount_check_oracle";

/// A volume with one 64 KiB extent shared by three files.
fn shared_volume(tag: &str) -> scratch::Volume {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-{tag}.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -m reflink=1 {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        xfs_io -f -c 'pwrite -q -S 0x5a 0 64k' -c fsync "$m/a"
        cp --reflink=always "$m/a" "$m/b" && cp --reflink=always "$m/a" "$m/c" && echo SHARED_OK
        sync
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK") && built.contains("SHARED_OK"),
        "building the shared volume failed:\n{built}"
    );
    volume
}

/// The record of AG 0's refcount btree root whose count is 3: its index,
/// as `xfs_db` numbers it, and its start block.
fn shared_record(image: &str) -> (usize, u64) {
    let shown = oracle("xfs_db")
        .args(["-r", "-c", "agf 0", "-c", "addr refcntroot", "-c", "p recs"])
        .arg(image)
        .output();
    // `recs[1-N] = [startblock,blockcount,refcount,cowflag] 1:[23,16,3,0] ...`
    for item in shown.stdout.split_whitespace() {
        let Some((n, fields)) = item.split_once(":[") else {
            continue;
        };
        let fields: Vec<&str> = fields.trim_end_matches(']').split(',').collect();
        if let (Ok(index), [start, _, count, ..]) = (n.parse::<usize>(), fields.as_slice()) {
            if *count == "3" {
                return (index, start.parse().expect("startblock"));
            }
        }
    }
    panic!("no record with a count of 3:\n{}", shown.stdout)
}

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

fn damage(volume: &scratch::Volume, commands: &[String]) {
    let mut db = oracle("xfs_db").args(["-x", "-c", "agf 0", "-c", "addr refcntroot"]);
    for c in commands {
        db = db.args(["-c", c]);
    }
    let edit = db.arg(volume.path()).output();
    assert!(
        edit.ok(),
        "xfs_db could not make the damage:\n{}{}",
        edit.stdout,
        edit.stderr
    );
}

fn fsck(image: &str) -> (Option<i32>, String) {
    let out = tool("fsck.xfs")
        .args(["--text", image])
        .output()
        .expect("spawn fsck.xfs");
    (
        out.status.code(),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

fn assert_found(name: &str, volume: &scratch::Volume, code: &str) {
    let image = volume.path().to_str().unwrap();
    let out = oracle("xfs_repair").args(["-n", image]).output();
    let report = out.repair_report();
    assert!(
        !repair::was_blind(&report),
        "{name}: xfs_repair -n declined to look:\n{report}"
    );
    assert!(
        !out.ok(),
        "{name}: xfs_repair -n calls this clean, so it is not damage:\n{report}"
    );
    let (status, text) = fsck(image);
    assert_eq!(status, Some(4), "{name}: fsck.xfs exit\n{text}");
    assert!(
        text.contains(&format!(": {code}: ")),
        "{name}: fsck.xfs did not report {code}; xfs_repair -n said:\n{report}\nfsck.xfs said:\n{text}"
    );
}

#[test]
fn a_volume_with_blocks_shared_three_ways_is_clean() {
    let volume = shared_volume("clean");
    let image = volume.path().to_str().unwrap();
    shared_record(image);
    let out = oracle("xfs_repair").args(["-n", image]).output();
    repair::assert_agreed(&out.repair_report(), "the shared volume");
    let (status, text) = fsck(image);
    assert_eq!(
        status,
        Some(0),
        "fsck.xfs calls the shared volume damaged:\n{text}"
    );
}

#[test]
fn a_count_lower_than_the_mappings_is_found() {
    let volume = shared_volume("count");
    let (index, _) = shared_record(volume.path().to_str().unwrap());
    damage(&volume, &[format!("write -d recs[{index}].refcount 2")]);
    assert_found("count lowered to 2", &volume, "refcount.count");
}

#[test]
fn a_record_moved_onto_free_space_is_found() {
    let volume = shared_volume("stale");
    let image = volume.path().to_str().unwrap();
    let (index, start) = shared_record(image);
    // The last free extent lies past every allocation, so the record
    // stays in order and only what it describes is wrong.
    let numrecs = db_number(image, &["agf 0", "addr bnoroot", "p numrecs"]);
    let free = db_number(
        image,
        &[
            "agf 0",
            "addr bnoroot",
            &format!("p recs[{numrecs}].startblock"),
        ],
    );
    assert!(
        free > start,
        "the last free extent is not past the shared extent"
    );
    damage(
        &volume,
        &[
            format!("write -d recs[{index}].startblock {free}"),
            format!("write -d recs[{index}].blockcount 1"),
        ],
    );
    assert_found("record moved onto free space", &volume, "refcount.stale");
}
