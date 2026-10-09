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

/// One damage per checker code (#364): `(code, case, xfs_db commands)`.
///
/// Each is damage `xfs_repair -n` must find, and the report must carry
/// the code it is named for, whatever else the damage also breaks. Unlike
/// the golden cases above, the rest of the report is not pinned: these
/// prove that a check exists and fires, not what it says around it.
///
/// `docs/fsck-output.md` lists every code, the case here (or above) that
/// damages it, or why none does; `tests/fsck_coverage_contract.rs`
/// holds the two to each other.
fn one_damage_per_code(base: &str) -> Vec<(&'static str, &'static str, Vec<String>)> {
    let ino = |name: &str| inode_of(base, name);
    let (small, medium) = (ino("small.txt"), ino("medium.bin"));
    let (fragmented, manyfiles) = (ino("fragmented.bin"), ino("manyfiles"));
    let sub = ino("sub");
    let root = db_value(base, &["sb 0", "p rootino"]);
    // A free inode in the root's chunk: the fixture allocates its first
    // inodes in order and leaves the end of the chunk free.
    let free_ino = root + 60;
    let first_free = db_value(base, &["agf 0", "addr bnoroot", "p recs[0].startblock"]);
    vec![
        (
            "identity",
            "bnobt-owner",
            vec![
                "agf 1".into(),
                "addr bnoroot".into(),
                "write -d owner 0".into(),
            ],
        ),
        (
            "sb.copy.unreadable",
            "sb1-magic",
            vec!["sb 1".into(), "write -d magicnum 0".into()],
        ),
        (
            "sb.copy.field",
            "sb1-logblocks",
            vec!["sb 1".into(), "write -d logblocks 1234".into()],
        ),
        (
            "ag.agi.unreadable",
            "agi1-magic",
            vec!["agi 1".into(), "write -d magicnum 0".into()],
        ),
        (
            "ag.agfl.unreadable",
            "agfl1-magic",
            vec!["agfl 1".into(), "write -d magicnum 0".into()],
        ),
        (
            "ag.length",
            "agi1-length",
            vec!["agi 1".into(), "write -d length 1000".into()],
        ),
        (
            "btree.unreadable",
            "cntbt-magic",
            vec![
                "agf 1".into(),
                "addr cntroot".into(),
                "write -d magic 0".into(),
            ],
        ),
        (
            "inobt.record",
            "inobt-freecount",
            vec![
                "agi 0".into(),
                "addr root".into(),
                "write -d recs[0].freecount 70".into(),
            ],
        ),
        (
            "inobt.chunk-count",
            "inobt-count",
            vec![
                "agi 0".into(),
                "addr root".into(),
                "write -d recs[0].count 32".into(),
            ],
        ),
        (
            "finobt.mismatch",
            "finobt-freecount",
            vec![
                "agi 0".into(),
                "addr free_root".into(),
                "write -d recs[0].freecount 3".into(),
            ],
        ),
        (
            "freesp.empty",
            "bnobt-empty",
            vec![
                "agf 1".into(),
                "addr bnoroot".into(),
                "write -d recs[0].blockcount 0".into(),
            ],
        ),
        (
            "freesp.overlap",
            "bnobt-overlap",
            vec![
                "agf 0".into(),
                "addr bnoroot".into(),
                format!("write -d recs[1].startblock {first_free}"),
            ],
        ),
        (
            "freesp.cnt-order",
            "cntbt-order",
            vec![
                "agf 0".into(),
                "addr cntroot".into(),
                "write -d recs[0].blockcount 99999".into(),
            ],
        ),
        (
            "counter.agf.btreeblks",
            "agf1-btreeblks",
            vec!["agf 1".into(), "write -d btreeblks 7".into()],
        ),
        (
            "counter.agi.inodes",
            "agi1-count",
            vec!["agi 1".into(), "write -d count 64".into()],
        ),
        (
            "counter.agi.iblocks",
            "agi1-iblocks",
            vec!["agi 1".into(), "write -d iblocks 5".into()],
        ),
        (
            "counter.agi.fblocks",
            "agi1-fblocks",
            vec!["agi 1".into(), "write -d fblocks 5".into()],
        ),
        (
            "counter.sb.ifree",
            "sb-ifree",
            vec!["sb 0".into(), "write -d ifree 99999".into()],
        ),
        (
            "range.block",
            "bnobt-past-end",
            vec![
                "agf 1".into(),
                "addr bnoroot".into(),
                "write -d recs[0].startblock 4000000".into(),
            ],
        ),
        (
            "extent.unreadable",
            "bmbt-magic",
            vec![
                format!("inode {fragmented}"),
                "addr u3.bmbt.ptrs[1]".into(),
                "write -d magic 0".into(),
            ],
        ),
        (
            "inode.unreadable",
            "inode-magic",
            vec![format!("inode {small}"), "write -d core.magic 0".into()],
        ),
        (
            "inode.free-in-use",
            "free-inode-mode",
            vec![
                format!("inode {free_ino}"),
                "write -d core.mode 0100644".into(),
            ],
        ),
        (
            "inode.allocated-unused",
            "inode-mode-zero",
            vec![format!("inode {medium}"), "write -d core.mode 0".into()],
        ),
        (
            "dir.root",
            "sb-rootino",
            vec!["sb 0".into(), format!("write -d rootino {free_ino}")],
        ),
        (
            "dir.not-a-directory",
            "root-mode",
            vec![format!("inode {root}"), "write -d core.mode 0100755".into()],
        ),
        (
            "dir.unreadable",
            "dir-data-magic",
            vec![
                format!("inode {manyfiles}"),
                "dblock 0".into(),
                "write -d dhdr.hdr.magic 0".into(),
            ],
        ),
        (
            "dir.reached-twice",
            "dir-two-parents",
            vec![
                format!("inode {sub}"),
                format!("write -d u3.sfdir3.list[0].inumber.i4 {manyfiles}"),
            ],
        ),
    ]
}

#[test]
fn every_code_fires_on_damage_the_reference_finds() {
    let source = fixture("xfsdata-default.img");
    let base = source.to_str().unwrap();
    let mut wrong = Vec::new();
    let cases = one_damage_per_code(base);
    for (code, name, commands) in &cases {
        let image = damaged(&source, name, commands);
        let image = image.path().to_str().unwrap();
        let (clean, said) = repair(image);
        if clean {
            wrong.push(format!(
                "{code} ({name}): xfs_repair -n finds nothing wrong after {commands:?}"
            ));
            continue;
        }
        let got = report_of(image);
        let codes: Vec<&str> = got.findings.iter().map(|r| r.code.as_str()).collect();
        // A traversal that failed is never a clean verdict.
        if got.clean || got.exit != Some(4) {
            wrong.push(format!(
                "{code} ({name}): xfs_repair -n finds damage and fsck.xfs exits {:?}, clean {}:\n{}\n--- xfs_repair -n:\n{said}",
                got.exit, got.clean, got.json
            ));
        } else if !codes.contains(code) {
            wrong.push(format!(
                "{code} ({name}): the report has {codes:?} and not {code}:\n{}",
                got.json
            ));
        }
    }
    // Every mismatch at once, so one run shows them all.
    assert!(
        wrong.is_empty(),
        "{} of {} cases disagree with the reference:\n\n{}",
        wrong.len(),
        cases.len(),
        wrong.join("\n\n")
    );
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
