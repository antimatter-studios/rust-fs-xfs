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
//! claiming one block, an extent outside every group, a directory entry
//! naming a free inode, and an entry whose recorded type is not its
//! inode's.
//!
//! **The JSON report is golden (#363).** For each damage the whole list of
//! findings -- code, severity and location -- is pinned, because a script
//! reading the report matches on exactly those, and a code that drifts is
//! a break no human reader would notice. A clean volume reports a complete
//! scan with no findings; a group whose header cannot be read reports a
//! partial one; a volume that cannot be mounted reports no scan at all.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, oracle, scratch};
use saphyr::{LoadableYamlNode, Yaml};

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

/// One finding as the JSON report lists it: code, severity, group, block
/// within the group, inode. `what` is left out on purpose -- it is for a
/// person, and may be reworded.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    code: String,
    severity: String,
    ag: Option<u64>,
    agbno: Option<u64>,
    ino: Option<u64>,
}

fn row(code: &str, ag: Option<u64>, agbno: Option<u64>, ino: Option<u64>) -> Row {
    Row {
        code: code.into(),
        severity: "error".into(),
        ag,
        agbno,
        ino,
    }
}

/// The JSON report `fsck.xfs` writes for `image`.
struct Report {
    exit: Option<i32>,
    schema: String,
    schema_version: i64,
    clean: bool,
    scan: String,
    findings: Vec<Row>,
    suppressed: i64,
    json: String,
}

fn report_of(image: &str) -> Report {
    let out = tool("fsck.xfs")
        .arg(image)
        .output()
        .expect("spawn fsck.xfs");
    let json = stdout(&out);
    let docs = Yaml::load_from_str(&json)
        .unwrap_or_else(|e| panic!("fsck.xfs {image} wrote no JSON ({e}):\n{json}"));
    let doc = docs
        .first()
        .unwrap_or_else(|| panic!("empty report:\n{json}"));
    let key = |node: &Yaml, name: &str| -> Option<String> {
        let v = node
            .as_mapping_get(name)
            .unwrap_or_else(|| panic!("no {name:?} in the report:\n{json}"));
        if v.is_null() {
            None
        } else if let Some(n) = v.as_integer() {
            Some(n.to_string())
        } else if let Some(b) = v.as_bool() {
            Some(b.to_string())
        } else {
            Some(
                v.as_str()
                    .unwrap_or_else(|| panic!("{name}: {v:?}"))
                    .to_string(),
            )
        }
    };
    let number = |node: &Yaml, name: &str| key(node, name).map(|v| v.parse::<u64>().unwrap());
    let findings = doc
        .as_mapping_get("findings")
        .and_then(Yaml::as_sequence)
        .unwrap_or_else(|| panic!("no findings list:\n{json}"))
        .iter()
        .map(|f| Row {
            code: key(f, "code").expect("a code"),
            severity: key(f, "severity").expect("a severity"),
            ag: number(f, "ag"),
            agbno: number(f, "agbno"),
            ino: number(f, "ino"),
        })
        .collect();
    Report {
        exit: out.status.code(),
        schema: key(doc, "schema").unwrap_or_default(),
        schema_version: key(doc, "schema_version")
            .and_then(|v| v.parse().ok())
            .unwrap_or(-1),
        clean: key(doc, "clean").as_deref() == Some("true"),
        scan: key(doc, "scan").unwrap_or_default(),
        findings,
        suppressed: key(doc, "suppressed")
            .and_then(|v| v.parse().ok())
            .unwrap_or(-1),
        json,
    }
}

/// `xfs_repair -n` on `image`: whether it found anything, and its report.
///
/// Read through the shared check (#124): a report saying the tool ignored
/// the log is neither clean nor damage, and is refused rather than counted.
fn repair(image: &str) -> (bool, String) {
    let out = oracle("xfs_repair").args(["-n", image]).output();
    let report = out.repair_report();
    assert!(
        !common::repair::was_blind(&report),
        "xfs_repair -n declined to look at {image}: its log holds records\n{report}"
    );
    (out.ok(), report)
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
        let report = oracle("xfs_repair")
            .args(["-n", image])
            .output()
            .repair_report();
        common::repair::assert_agreed(&report, &format!("{name}: the clean fixture"));
        let (code, said) = fsck(image);
        assert_eq!(
            code,
            Some(0),
            "{name}: fsck.xfs finds fault with a clean volume:\n{said}"
        );
        let got = report_of(image);
        assert_eq!(
            (
                got.schema.as_str(),
                got.schema_version,
                got.clean,
                got.scan.as_str(),
                got.findings.len(),
                got.suppressed
            ),
            ("rust-fs-xfs/fsck", 1, true, "complete", 0, 0),
            "{name}: a clean volume is a complete scan with nothing found:\n{}",
            got.json
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

    let number = |v: &str| -> u64 {
        v.parse()
            .unwrap_or_else(|_| panic!("not an inode number: {v:?}"))
    };
    let (small_ino, medium_ino, large_ino) = (number(&small), number(&medium), number(&large));
    let first_entry = db_value(
        base,
        &[&format!("inode {root}"), "p u3.sfdir3.list[0].inumber.i4"],
    );
    // Where medium.bin's data starts: the blocks lost when its extent is
    // pointed somewhere else. Group 0, so the block is the group block.
    let medium_start = db_value(
        base,
        &[&format!("inode {medium}"), "p u3.bmx[0].startblock"],
    );
    // The second free extent by block, whose tail is lost when the record
    // is shortened to one block.
    let bno_start = db_value(base, &["agf 0", "addr bnoroot", "p recs[1].startblock"]);
    let shared_bno: u64 = shared.parse().expect("a block number");

    let ag0 = Some(0);
    // (case, the damage, the findings fsck.xfs must report, the scan.)
    let cases: Vec<(&str, Vec<String>, Vec<Row>, &str)> = vec![
        (
            "agf-freeblks",
            vec!["agf 0".into(), "write -d freeblks 1".into()],
            vec![row("counter.agf.freeblks", ag0, None, None)],
            "complete",
        ),
        (
            "agf-longest",
            vec!["agf 0".into(), "write -d longest 1".into()],
            vec![row("counter.agf.longest", ag0, None, None)],
            "complete",
        ),
        (
            "agi-freecount",
            vec!["agi 0".into(), "write -d freecount 1000".into()],
            // This exceeds the AGI's count, so mount rejects the header
            // before the checker can compare it with the inode btree.
            vec![row("mount", None, None, None)],
            "none",
        ),
        (
            "sb-icount",
            vec!["sb 0".into(), "write -d icount 99999".into()],
            vec![row("counter.sb.icount", None, None, None)],
            "complete",
        ),
        (
            "bno-record",
            vec![
                "agf 0".into(),
                "addr bnoroot".into(),
                "write -d recs[1].blockcount 1".into(),
            ],
            vec![
                row("freesp.disagree", ag0, None, None),
                row("counter.agf.freeblks", ag0, None, None),
                row("lost", ag0, Some(bno_start + 1), None),
                row("counter.sb.fdblocks", None, None, None),
            ],
            "complete",
        ),
        (
            "link-count",
            vec![format!("inode {small}"), "write -d core.nlinkv2 5".into()],
            vec![row("inode.nlink", ag0, None, Some(small_ino))],
            "complete",
        ),
        (
            "inode-checksum",
            vec![format!("inode {medium}"), "write -c core.size 12345".into()],
            vec![
                row("checksum", ag0, None, Some(medium_ino)),
                row("dir.entry-target", ag0, None, Some(root)),
                row("lost", ag0, Some(medium_start), None),
            ],
            "partial",
        ),
        (
            "cross-link",
            vec![
                format!("inode {medium}"),
                format!("write -d u3.bmx[0].startblock {shared}"),
            ],
            vec![
                row(
                    "cross-link",
                    ag0,
                    Some(shared_bno),
                    Some(medium_ino.max(large_ino)),
                ),
                row("lost", ag0, Some(medium_start), None),
            ],
            "complete",
        ),
        (
            "extent-out-of-range",
            vec![
                format!("inode {medium}"),
                "write -d u3.bmx[0].startblock 4503599627370495".into(),
            ],
            vec![
                row("range.extent", ag0, None, Some(medium_ino)),
                row("lost", ag0, Some(medium_start), None),
            ],
            "complete",
        ),
        (
            "entry-to-free-inode",
            vec![
                format!("inode {root}"),
                format!("write -d u3.sfdir3.list[0].inumber.i4 {free_ino}"),
            ],
            vec![
                row("dir.entry-target", ag0, None, Some(root)),
                row("dir.unreached", ag0, None, Some(first_entry)),
            ],
            "complete",
        ),
        (
            "entry-type",
            vec![
                format!("inode {root}"),
                "write -d u3.sfdir3.list[0].filetype 2".into(),
            ],
            vec![row("dir.entry-ftype", ag0, None, Some(root))],
            "complete",
        ),
    ];

    for (name, commands, mut want, scan) in cases {
        let image = damaged(&source, name, &commands);
        let image = image.path().to_str().unwrap();

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

        let got = report_of(image);
        let mut rows = got.findings.clone();
        rows.sort();
        want.sort();
        assert_eq!(
            (rows, got.scan.as_str(), got.clean, got.exit),
            (want, scan, false, Some(4)),
            "{name}: the JSON report is not the one pinned for {commands:?}:\n{}",
            got.json
        );
    }
}

/// A copy of `source` damaged by `commands`, run through `xfs_db -x`.
fn damaged(source: &std::path::Path, name: &str, commands: &[String]) -> scratch::Volume {
    let copy =
        scratch::Volume::copy_of(SUITE, source, &format!("{}-{name}.img", std::process::id()));
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

/// The number `xfs_db -r` prints for the last of `commands` on `image`.
fn db_value(image: &str, commands: &[&str]) -> u64 {
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

#[test]
fn a_group_that_cannot_be_read_is_a_partial_scan() {
    let source = fixture("xfsdata-default.img");
    let last = db_value(source.to_str().unwrap(), &["sb 0", "p agcount"]) - 1;
    let commands = vec![format!("agf {last}"), "write -d magicnum 0".into()];
    let image = damaged(&source, "agf-magic", &commands);
    let image = image.path().to_str().unwrap();
    let (clean, said) = repair(image);
    assert!(
        !clean,
        "xfs_repair -n finds nothing wrong with {commands:?}:\n{said}"
    );
    let got = report_of(image);
    assert_eq!(
        (got.scan.as_str(), got.clean, got.exit),
        ("partial", false, Some(4)),
        "a group whose AGF cannot be read is not a complete scan:\n{}",
        got.json
    );
    assert!(
        got.findings
            .contains(&row("ag.agf.unreadable", Some(last), None, None)),
        "no ag.agf.unreadable for group {last}:\n{}",
        got.json
    );
}

#[test]
fn a_volume_that_cannot_be_mounted_reports_no_scan() {
    let source = fixture("xfsdata-default.img");
    let commands = vec!["sb 0".into(), "write -d blocklog 10".into()];
    let image = damaged(&source, "sb-blocklog", &commands);
    let image = image.path().to_str().unwrap();
    let (clean, said) = repair(image);
    assert!(
        !clean,
        "xfs_repair -n finds nothing wrong with {commands:?}:\n{said}"
    );
    let got = report_of(image);
    assert_eq!(
        (got.findings.clone(), got.scan.as_str(), got.clean, got.exit),
        (vec![row("mount", None, None, None)], "none", false, Some(4)),
        "a volume that cannot be mounted is a finding with no scan behind it:\n{}",
        got.json
    );
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
fn a_repair_of_a_clean_volume_writes_nothing_and_asking_for_both_is_refused() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-default.img"),
        &format!("{}-y.img", std::process::id()),
    );
    let path = copy.path().to_str().unwrap();
    let before = std::fs::read(path).unwrap();
    let out = tool("fsck.xfs").args(["-y", path]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "fsck.xfs -y: {}", stderr(&out));
    assert!(
        std::fs::read(path).unwrap() == before,
        "fsck.xfs -y changed a clean volume"
    );
    for both in [["-n", "-y"], ["-y", "--dry-run"]] {
        let out = tool("fsck.xfs").args(both).arg(path).output().unwrap();
        assert_eq!(
            out.status.code(),
            Some(16),
            "fsck.xfs {both:?}: {}",
            stderr(&out)
        );
    }
}
