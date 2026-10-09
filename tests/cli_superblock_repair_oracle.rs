//! `fsck.xfs -y` rewrites one damaged secondary superblock from the
//! primary, and `xfs_repair -n` and `xfs_db` agree it did (#391).
//!
//! Each case damages a copy of the data fixture with `xfs_db -x`:
//!
//! - one field of group 1's copy, its checksum stamped again, and group
//!   2's copy with its checksum broken: `xfs_repair -n` must find each,
//!   `fsck.xfs -y` must repair it (exit 1), `xfs_repair -n` must then
//!   call the volume clean and `xfs_db` read the copy's field as the
//!   primary's. A second `fsck.xfs -y` writes nothing (exit 0), and no
//!   byte outside the copy changed, so every file reads as it did.
//! - a copy naming another filesystem, and two damaged copies: the repair
//!   is refused (exit 4) and the volume is byte for byte what it was.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, oracle, scratch};

const SUITE: &str = "cli_superblock_repair_oracle";

fn damaged(name: &str, commands: &[&str]) -> scratch::Volume {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfsdata-default.img"),
        &format!("{}-{name}.img", std::process::id()),
    );
    let mut db = oracle("xfs_db").arg("-x");
    for c in commands {
        db = db.args(["-c", c]);
    }
    let edit = db.arg(copy.path().to_str().unwrap()).output();
    assert!(
        edit.ok(),
        "{name}: xfs_db could not make the damage {commands:?}:\n{}{}",
        edit.stdout,
        edit.stderr
    );
    copy
}

fn fsck_y(image: &str) -> (Option<i32>, String) {
    let out = tool("fsck.xfs")
        .args(["--text", "-y", image])
        .output()
        .unwrap();
    (
        out.status.code(),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

/// What `xfs_db -r` prints for `field` in superblock `n`.
fn sb_field(image: &str, n: u32, field: &str) -> String {
    let shown = oracle("xfs_db")
        .args([
            "-r",
            "-c",
            &format!("sb {n}"),
            "-c",
            &format!("p {field}"),
            image,
        ])
        .output();
    shown.stdout.trim().to_string()
}

/// Group `ag`'s copy: where it starts and how long it is.
fn copy_range(image: &str, ag: u64) -> (usize, usize) {
    let num = |field: &str| -> u64 {
        sb_field(image, 0, field)
            .split('=')
            .nth(1)
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("xfs_db printed no {field}"))
    };
    let at = ag * num("agblocks") * num("blocksize");
    (at as usize, num("sectsize") as usize)
}

#[test]
fn one_damaged_copy_is_rewritten_and_the_reference_agrees() {
    for (name, ag, commands) in [
        ("field", 1u64, vec!["sb 1", "write -d logblocks 1234"]),
        ("checksum", 2, vec!["sb 2", "write -c logblocks 1234"]),
    ] {
        let volume = damaged(name, &commands);
        let image = volume.path().to_str().unwrap();
        let report = oracle("xfs_repair").args(["-n", image]).output();
        assert!(
            !report.ok(),
            "{name}: xfs_repair -n finds nothing wrong after {commands:?}:\n{}",
            report.repair_report()
        );
        let before = std::fs::read(image).unwrap();

        let (code, said) = fsck_y(image);
        assert_eq!(code, Some(1), "{name}: fsck.xfs -y did not repair:\n{said}");
        let after = oracle("xfs_repair").args(["-n", image]).output();
        common::repair::assert_agreed(
            &after.repair_report(),
            &format!("{name}: after fsck.xfs -y"),
        );
        assert_eq!(
            sb_field(image, ag as u32, "logblocks"),
            sb_field(image, 0, "logblocks"),
            "{name}: xfs_db reads another logblocks in the copy than in the primary"
        );

        // Only the copy changed: every file's blocks read as they did.
        let repaired = std::fs::read(image).unwrap();
        let (at, len) = copy_range(image, ag);
        assert_eq!(repaired.len(), before.len());
        let outside = |v: &[u8]| [v[..at].to_vec(), v[at + len..].to_vec()].concat();
        assert!(
            outside(&repaired) == outside(&before),
            "{name}: fsck.xfs -y changed bytes outside group {ag}'s superblock copy"
        );

        // And again: nothing to do.
        let (code, said) = fsck_y(image);
        assert_eq!(code, Some(0), "{name}: a second fsck.xfs -y:\n{said}");
        assert!(
            std::fs::read(image).unwrap() == repaired,
            "{name}: a second fsck.xfs -y wrote"
        );
    }
}

#[test]
fn a_foreign_copy_or_two_damaged_copies_are_refused_and_nothing_is_written() {
    // (case, damage, whether xfs_repair -n calls it damage). It does not
    // compare a secondary superblock's UUID, so a copy naming another
    // filesystem is not damage to it; this checker reports it, and the
    // repair refuses to overwrite what it cannot identify, which is what
    // the case is for.
    for (name, commands, reference_damage) in [
        (
            "foreign",
            vec!["sb 1", "write -d uuid 01234567-89ab-cdef-0123-456789abcdef"],
            false,
        ),
        (
            "two",
            vec![
                "sb 1",
                "write -d logblocks 1234",
                "sb 3",
                "write -d logblocks 4321",
            ],
            true,
        ),
    ] {
        let volume = damaged(name, &commands);
        let image = volume.path().to_str().unwrap();
        let report = oracle("xfs_repair").args(["-n", image]).output();
        assert_eq!(
            !report.ok(),
            reference_damage,
            "{name}: xfs_repair -n does not say what this case expects of {commands:?}:\n{}",
            report.repair_report()
        );
        let before = std::fs::read(image).unwrap();
        let (code, said) = fsck_y(image);
        assert_eq!(code, Some(4), "{name}: fsck.xfs -y did not refuse:\n{said}");
        assert!(
            std::fs::read(image).unwrap() == before,
            "{name}: a refused repair wrote"
        );
    }
}
