//! `fsck.xfs -y` counts group and superblock counters again from their
//! trees, judged by xfs_db, xfs_repair and Linux (#392).
//!
//! Each damaged counter starts from an independent, kernel-made fixture.
//! A damaged value is one the mount still accepts (a free count above the
//! inode count is refused at mount, which is a different case): the repair
//! must restore the counter, change no other byte of the image, leave a
//! volume `xfs_repair -n` calls clean and Linux mounts, and find nothing
//! to do a second time. A counter beside a tree the check does not find
//! sound, or on a log that needs replay, refuses the repair and writes
//! nothing.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, oracle, repair, scratch};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

const SUITE: &str = "counter_repair_kernel";

fn digest(path: &Path) -> Vec<u8> {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = file.read(&mut buf).unwrap();
        if n == 0 {
            return hash.finalize().to_vec();
        }
        hash.update(&buf[..n]);
    }
}

fn db(image: &str, commands: &[&str], writable: bool) -> String {
    let mut cmd = oracle("xfs_db").arg(if writable { "-x" } else { "-r" });
    for command in commands {
        cmd = cmd.args(["-c", command]);
    }
    let out = cmd.arg(image).output();
    assert!(out.ok(), "xfs_db: {}{}", out.stdout, out.stderr);
    out.stdout
}

fn value(image: &str, header: &str, field: &str) -> String {
    let out = db(image, &[header, &format!("p {field}")], false);
    out.lines()
        .find_map(|line| line.strip_prefix(&format!("{field} = ")))
        .unwrap_or_else(|| panic!("xfs_db did not report {header} {field}: {out}"))
        .trim()
        .to_string()
}

fn run_repair(image: &str) -> std::process::Output {
    tool("fsck.xfs")
        .args(["--text", "-y", image])
        .output()
        .expect("run fsck.xfs -y")
}

fn clean(image: &str, context: &str) {
    let out = oracle("xfs_repair").args(["-n", image]).output();
    repair::assert_agreed(&out.repair_report(), context);
    assert!(out.ok(), "{context}: {}{}", out.stdout, out.stderr);
}

fn mounted(image: &str) {
    let out = kernel_run(&format!(
        r#"
        set -eu
        img=$(mktemp /var/tmp/counter-repair-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop "$img" "$m"; then
            ls -a "$m" >/dev/null
            echo MOUNT_OK
            umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        else
            echo MOUNT_REFUSED
            exit 1
        fi
        rmdir "$m"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    assert!(out.contains("MOUNT_OK"), "{out}");
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    repair::assert_agreed(&out, "the repaired image after Linux mounted it");
}

fn repaired_counter(source: &Path, tag: &str, header: &str, field: &str, bad: &str) {
    let copy =
        scratch::Volume::copy_of(SUITE, source, &format!("{}-{tag}.img", std::process::id()));
    let image = copy.path().to_str().unwrap();
    let expected = value(image, header, field);
    let original = digest(copy.path());
    db(image, &[header, &format!("write -d {field} {bad}")], true);
    assert_ne!(
        value(image, header, field),
        expected,
        "{tag}: no damage made"
    );
    let out = run_repair(image);
    assert_eq!(
        out.status.code(),
        Some(1),
        "{tag}: fsck.xfs -y must correct the counter (exit 1): {}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(value(image, header, field), expected, "{tag}");
    assert_eq!(
        digest(copy.path()),
        original,
        "{tag}: only the counter and CRC may change"
    );
    clean(image, tag);
    mounted(image);
    let again = run_repair(image);
    assert_eq!(
        again.status.code(),
        Some(0),
        "repeat: {}{}",
        stdout(&again),
        stderr(&again)
    );
}

#[test]
fn each_derivable_counter_is_repaired_independently() {
    let source = fixture("xfsfeat-finobt-inobtcount.img");
    for (header, field, bad) in [
        ("agf 0", "freeblks", "1"),
        ("agf 0", "longest", "1"),
        ("agf 0", "btreeblks", "99"),
        ("agi 0", "count", "4096"),
        ("agi 0", "freecount", "1"),
        ("agi 0", "iblocks", "77"),
        ("agi 0", "fblocks", "77"),
        ("sb 0", "icount", "99999"),
        ("sb 0", "ifree", "7"),
        ("sb 0", "fdblocks", "1"),
    ] {
        repaired_counter(&source, field, header, field, bad);
    }
}

#[test]
fn deep_free_space_trees_and_later_groups_are_counted() {
    repaired_counter(
        &fixture("xfsdeep-bno2.img"),
        "deep-btree",
        "agf 0",
        "btreeblks",
        "0",
    );
    repaired_counter(
        &fixture("xfs-default.img"),
        "last-group",
        "agf 3",
        "freeblks",
        "1",
    );
}

#[test]
fn ambiguous_or_untrusted_state_is_refused_without_any_write() {
    for (tag, commands) in [
        (
            "disagree",
            vec!["agf 0", "addr bnoroot", "write -d recs[1].blockcount 1"],
        ),
        (
            "crc",
            vec!["agf 0", "addr bnoroot", "write -c recs[1].blockcount 1"],
        ),
        ("owner", vec!["agf 0", "addr bnoroot", "write -d owner 1"]),
        (
            "free-inode-tree",
            vec!["agi 0", "addr free_root", "write -d recs[0].freecount 0"],
        ),
        ("free-list", vec!["agf 0", "write -d flcount 999"]),
    ] {
        let copy = scratch::Volume::copy_of(
            SUITE,
            &fixture("xfsfeat-finobt-inobtcount.img"),
            &format!("{}-{tag}.img", std::process::id()),
        );
        let image = copy.path().to_str().unwrap();
        db(image, &["agf 0", "write -d freeblks 1"], true);
        db(image, &commands, true);
        let before = digest(copy.path());
        let out = run_repair(image);
        assert_eq!(
            out.status.code(),
            Some(4),
            "{tag}: {}{}",
            stdout(&out),
            stderr(&out)
        );
        assert_eq!(
            digest(copy.path()),
            before,
            "{tag}: refusal changed the device"
        );
    }
    let copy = scratch::Volume::copy_of(SUITE, &fixture("xfsdirty.img"), "dirty.img");
    let before = digest(copy.path());
    let out = run_repair(copy.path().to_str().unwrap());
    assert_eq!(
        out.status.code(),
        Some(4),
        "{}{}",
        stdout(&out),
        stderr(&out)
    );
    assert_eq!(
        digest(copy.path()),
        before,
        "a dirty log must not be repaired"
    );
}
