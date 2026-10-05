//! `fsck.xfs` agrees with `xfs_repair -n` about what is wrong (#339).
//!
//! Two halves, each against the reference tool in the harness guest:
//!
//! - **Clean volumes stay clean.** Every fixture below was made and filled
//!   by the standard formatter and the kernel. `xfs_repair -n` must call
//!   each clean, and so must `fsck.xfs` (exit 0). A checker that cries
//!   wolf on a healthy volume is worse than none.
//! - **Damage is found.** A copy of the data fixture is damaged one way at
//!   a time with `xfs_db -x`, which recomputes the checksum of whatever it
//!   edits unless told not to, so each case is the damage it names and not
//!   a checksum failure. `xfs_repair -n` must find each one (status 1), or
//!   the case is not damage and the test says so; then `fsck.xfs` must
//!   find it too (exit 4).
//!
//! The cases cover each family of check: the AGF and AGI counters against
//! the trees, a free-space record the two free-space trees disagree on,
//! the superblock's inode count, a link count, a checksum, two files
//! claiming one block, a directory entry naming a free inode, and an entry
//! whose recorded type is not its inode's.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, oracle, scratch};

const SUITE: &str = "cli_fsck_oracle";

/// Fixtures the reference tools call clean.
const CLEAN: &[&str] = &[
    "xfs-default.img",
    "xfs-1k.img",
    "xfs-2k.img",
    "xfs-reflink.img",
    "xfs-bigtime.img",
    "xfs-nosparse.img",
    "xfsdata-default.img",
    "xfsdata-1k.img",
    "xfsdata-ftype.img",
    "xfscli-v5.img",
    "xfsfeat-base.img",
    "xfsfeat-everything.img",
];

/// `fsck.xfs` on `image`: its exit status and what it printed.
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

/// `xfs_repair -n` on `image`: its exit status and report.
fn repair(image: &str) -> (bool, String) {
    let out = oracle("xfs_repair").args(["-n", image]).output();
    (out.ok(), format!("{}{}", out.stdout, out.stderr))
}

#[test]
fn every_volume_the_reference_calls_clean_is_clean() {
    for name in CLEAN {
        let copy = scratch::Volume::copy_of(
            SUITE,
            &fixture(name),
            &format!("{}-{name}", std::process::id()),
        );
        let image = copy.path().to_str().unwrap();
        let (clean, report) = repair(image);
        assert!(
            clean,
            "{name}: xfs_repair -n does not call the fixture clean:\n{report}"
        );
        let (code, said) = fsck(image);
        assert_eq!(
            code,
            Some(0),
            "{name}: fsck.xfs finds fault with a clean volume:\n{said}"
        );
    }
}

/// The inode number `fs.xfs ls /` reports for `name`.
fn inode_of(image: &str, name: &str) -> String {
    let listing = stdout(&ok(tool("fs.xfs").args([image, "ls", "/"])));
    let at = listing
        .find(&format!("\"name\": \"{name}\""))
        .unwrap_or_else(|| panic!("no {name} in the data fixture:\n{listing}"));
    json_field(&listing[at..], "inode")
}

#[test]
fn every_damage_the_reference_finds_is_found() {
    let source = fixture("xfsdata-default.img");
    let base = source.to_str().unwrap();
    let small = inode_of(base, "small.txt");
    let medium = inode_of(base, "medium.bin");
    let large = inode_of(base, "large.bin");
    // Where large.bin's first extent starts, so medium.bin can be pointed
    // at the same blocks.
    let shown = oracle("xfs_db")
        .args([
            "-r",
            "-c",
            &format!("inode {large}"),
            "-c",
            "p u3.bmx[0].startblock",
            base,
        ])
        .output();
    let shared = shown
        .stdout
        .split('=')
        .nth(1)
        .unwrap_or_else(|| {
            panic!(
                "xfs_db printed no startblock for large.bin:\n{}",
                shown.stdout
            )
        })
        .trim()
        .to_string();
    let root_shown = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "p rootino", base])
        .output();
    let root: u64 = root_shown
        .stdout
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("xfs_db printed no rootino:\n{}", root_shown.stdout));
    // A free inode in the root's own chunk: the fixture allocates the
    // first eleven of its 64 in order and leaves the rest free.
    let free_ino = (root + 22).to_string();

    let cases: Vec<(&str, Vec<String>)> = vec![
        (
            "agf-freeblks",
            vec!["agf 0".into(), "write -d freeblks 1".into()],
        ),
        (
            "agf-longest",
            vec!["agf 0".into(), "write -d longest 1".into()],
        ),
        (
            "agi-freecount",
            vec!["agi 0".into(), "write -d freecount 1000".into()],
        ),
        (
            "sb-icount",
            vec!["sb 0".into(), "write -d icount 99999".into()],
        ),
        (
            "bno-record",
            vec![
                "agf 0".into(),
                "addr bnoroot".into(),
                "write -d recs[1].blockcount 1".into(),
            ],
        ),
        (
            "link-count",
            vec![format!("inode {small}"), "write -d core.nlinkv2 5".into()],
        ),
        (
            "inode-checksum",
            vec![format!("inode {medium}"), "write -c core.size 12345".into()],
        ),
        (
            "cross-link",
            vec![
                format!("inode {medium}"),
                format!("write -d u3.bmx[0].startblock {shared}"),
            ],
        ),
        (
            "entry-to-free-inode",
            vec![
                format!("inode {root}"),
                format!("write -d u3.sfdir3.list[0].inumber.i4 {free_ino}"),
            ],
        ),
        (
            "entry-type",
            vec![
                format!("inode {root}"),
                "write -d u3.sfdir3.list[0].filetype 2".into(),
            ],
        ),
    ];

    for (name, commands) in cases {
        let copy = scratch::Volume::copy_of(
            SUITE,
            &source,
            &format!("{}-{name}.img", std::process::id()),
        );
        let image = copy.path().to_str().unwrap();
        let mut db = oracle("xfs_db").arg("-x");
        for c in &commands {
            db = db.args(["-c", c]);
        }
        let edit = db.arg(image).output();
        assert!(
            edit.ok(),
            "{name}: xfs_db could not make the damage {commands:?}:\n{}{}",
            edit.stdout,
            edit.stderr
        );

        let (clean, report) = repair(image);
        assert!(
            !clean,
            "{name}: xfs_repair -n finds nothing wrong, so {commands:?} is not damage:\n{report}"
        );

        let (code, said) = fsck(image);
        assert_eq!(
            code,
            Some(4),
            "{name}: xfs_repair -n finds the damage {commands:?} and fsck.xfs does not:\n{said}\n\
             --- xfs_repair -n said:\n{report}"
        );
    }
}

#[test]
fn a_volume_that_is_not_xfs_is_an_operational_error() {
    let volume =
        scratch::Volume::empty(SUITE, &format!("{}-zero.img", std::process::id()), 1 << 20);
    let (code, said) = fsck(volume.path().to_str().unwrap());
    assert_eq!(
        code,
        Some(8),
        "a zeroed device is not XFS, and nothing was checked:\n{said}"
    );
}

#[test]
fn a_repair_is_refused_not_pretended() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-default.img"),
        &format!("{}-y.img", std::process::id()),
    );
    let out = tool("fsck.xfs")
        .args(["-y", copy.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(16), "fsck.xfs -y: {}", stderr(&out));
}
