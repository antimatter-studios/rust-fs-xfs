//! The salvage reads of superblocks and group headers, against `xfs_db`
//! and `xfs_repair -n` in the harness guest.
//!
//! Two claims. Undamaged, every group header decodes to the fields
//! `xfs_db` prints for it. Damaged, by `xfs_db -x` writing a field with a
//! fresh checksum (`write -d`) or a stale one (`write -c`), the salvage
//! read names the structure `xfs_db` damaged and says how, and
//! `xfs_repair -n` calls the same copy unclean, so the finding is about
//! real damage and not about this crate's reading.

mod common;

use common::{fixture, oracle, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, Damage, DamageKind, Integrity};
use std::collections::HashMap;
use std::path::Path;

const SUITE: &str = "salvage_headers_oracle";

fn open(path: &Path) -> FileDevice {
    FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()))
}

/// `key = value` lines from `xfs_db`'s `print`, after the line `marker`.
fn printed(text: &str, marker: &str) -> HashMap<String, u64> {
    text.lines()
        .skip_while(|l| l.trim() != marker)
        .skip(1)
        .take_while(|l| !l.starts_with("--"))
        .filter_map(|l| {
            let (key, value) = l.split_once(" = ")?;
            let value = value.trim();
            // `null` is how xfs_db prints NULLAGINO and NULLAGBLOCK.
            let number = match value.strip_prefix("0x") {
                Some(hex) => u64::from_str_radix(hex, 16).ok()?,
                None if value == "null" => u64::from(u32::MAX),
                None => value.parse().ok()?,
            };
            Some((key.trim().to_string(), number))
        })
        .collect()
}

/// Every group header of two fixtures, v5 and v4, decodes to what `xfs_db`
/// prints for it.
#[test]
fn group_headers_decode_to_what_xfs_db_prints() {
    for name in ["xfs-default", "xfs-nocrc"] {
        let image = fixture(&format!("{name}.img"));
        let device = open(&image);
        let found = salvage::superblocks(&device);
        let sb = found.geometry.expect("a geometry");
        let mut commands = Vec::new();
        for ag in 0..sb.agcount {
            for header in ["agf", "agi"] {
                commands.extend([
                    "-c".to_string(),
                    format!("echo -- {header} {ag}"),
                    "-c".to_string(),
                    format!("{header} {ag}"),
                    "-c".to_string(),
                    "print".to_string(),
                ]);
            }
        }
        commands.extend(["-c".to_string(), "echo --".to_string()]);
        let out = oracle("xfs_db")
            .arg("-r")
            .args(&commands)
            .arg(image.to_str().unwrap())
            .output();
        assert!(
            out.ok(),
            "{name}: xfs_db failed:\n{}{}",
            out.stdout,
            out.stderr
        );
        for ag in 0..sb.agcount {
            let headers = salvage::ag_headers(&device, &sb, ag);
            assert!(
                headers.all_damage().next().is_none(),
                "{name}: group {ag}: {:#?}",
                headers.all_damage().collect::<Vec<_>>()
            );
            let agf = headers.agf.value.expect("AGF decoded");
            let db = printed(&out.stdout, &format!("-- agf {ag}"));
            for (field, ours) in [
                ("seqno", u64::from(agf.seqno)),
                ("length", u64::from(agf.length)),
                ("bnoroot", u64::from(agf.roots[0])),
                ("cntroot", u64::from(agf.roots[1])),
                ("bnolevel", u64::from(agf.levels[0])),
                ("cntlevel", u64::from(agf.levels[1])),
                ("flfirst", u64::from(agf.flfirst)),
                ("fllast", u64::from(agf.fllast)),
                ("flcount", u64::from(agf.flcount)),
                ("freeblks", u64::from(agf.freeblks)),
                ("longest", u64::from(agf.longest)),
                ("btreeblks", u64::from(agf.btreeblks)),
            ] {
                assert_eq!(
                    Some(&ours),
                    db.get(field),
                    "{name}: AGF {ag} {field}:\n{}",
                    out.stdout
                );
            }
            let agi = headers.agi.value.expect("AGI decoded");
            let db = printed(&out.stdout, &format!("-- agi {ag}"));
            for (field, ours) in [
                ("seqno", u64::from(agi.seqno)),
                ("length", u64::from(agi.length)),
                ("count", u64::from(agi.count)),
                ("root", u64::from(agi.root)),
                ("level", u64::from(agi.level)),
                ("freecount", u64::from(agi.freecount)),
                ("newino", u64::from(agi.newino)),
            ] {
                assert_eq!(
                    Some(&ours),
                    db.get(field),
                    "{name}: AGI {ag} {field}:\n{}",
                    out.stdout
                );
            }
        }
    }
}

/// A copy of the v5 default fixture with `commands` run by `xfs_db -x`.
fn damaged_by_xfs_db(tag: &str, commands: &[&str]) -> scratch::Volume {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-default.img"),
        &format!("{}-{tag}.img", std::process::id()),
    );
    let mut db = oracle("xfs_db").arg("-x");
    for command in commands {
        db = db.args(["-c", command]);
    }
    let out = db.arg(copy.path().to_str().unwrap()).output();
    assert!(
        out.ok(),
        "{tag}: xfs_db could not make the damage {commands:?}:\n{}{}",
        out.stdout,
        out.stderr
    );
    copy
}

/// `xfs_repair -n` must call `image` unclean, and its report is returned.
fn repair_calls_it_damaged(image: &Path, tag: &str) -> String {
    let out = oracle("xfs_repair")
        .args(["-n", image.to_str().unwrap()])
        .output();
    let report = out.repair_report();
    assert!(
        report.contains("REPAIR_RC=") && !report.contains("REPAIR_RC=0"),
        "{tag}: xfs_repair -n found nothing wrong, so the damage is not damage:\n{report}"
    );
    report
}

struct Case {
    tag: &'static str,
    commands: &'static [&'static str],
    structure: &'static str,
    kinds: &'static [DamageKind],
    /// What the report of `xfs_repair -n` names, lower-cased.
    repair_names: &'static str,
}

const CASES: &[Case] = &[
    Case {
        tag: "copy-field-stale-crc",
        commands: &["sb 1", "write -c logblocks 1234"],
        structure: "superblock 1",
        kinds: &[DamageKind::Checksum, DamageKind::Mismatch],
        repair_names: "superblock",
    },
    Case {
        tag: "copy-field-fresh-crc",
        commands: &["sb 2", "write -d logblocks 1234"],
        structure: "superblock 2",
        kinds: &[DamageKind::Mismatch],
        repair_names: "superblock",
    },
    Case {
        tag: "agf-magic",
        commands: &["agf 1", "write -d magicnum 0"],
        structure: "AGF 1",
        kinds: &[DamageKind::Signature],
        repair_names: "agf",
    },
    Case {
        tag: "agi-count-stale-crc",
        commands: &["agi 2", "write -c count 7"],
        structure: "AGI 2",
        kinds: &[DamageKind::Checksum],
        repair_names: "agi",
    },
    Case {
        tag: "agf-seqno-fresh-crc",
        commands: &["agf 3", "write -d seqno 2"],
        structure: "AGF 3",
        kinds: &[DamageKind::Mismatch],
        repair_names: "agf",
    },
];

/// Each damage `xfs_db` makes is found in the structure it was made in, of
/// the kinds it must be, and nowhere else; `xfs_repair -n` agrees the copy
/// is damaged.
#[test]
fn damage_xfs_db_makes_is_found_where_it_was_made() {
    for case in CASES {
        let copy = damaged_by_xfs_db(case.tag, case.commands);
        let report = salvage::damage(&open(copy.path()));
        let all: Vec<&Damage> = report.all().collect();
        assert!(
            all.iter().all(|d| d.structure == case.structure),
            "{}: damage outside {}: {all:#?}",
            case.tag,
            case.structure
        );
        let mut kinds: Vec<DamageKind> = all.iter().map(|d| d.kind).collect();
        kinds.dedup();
        assert_eq!(kinds, case.kinds, "{}: {all:#?}", case.tag);
        let repair = repair_calls_it_damaged(copy.path(), case.tag).to_lowercase();
        assert!(
            repair.contains(case.repair_names),
            "{}: xfs_repair -n does not name the {}:\n{repair}",
            case.tag,
            case.repair_names
        );
        // The geometry is the undamaged one whatever was done to a copy.
        let sb = report.superblocks.geometry.expect("a geometry");
        assert_eq!(report.superblocks.source, Some(0), "{}", case.tag);
        assert_eq!(sb.logblocks, 16384, "{}", case.tag);
        if case.structure.starts_with("superblock") {
            let ag: usize = case.structure["superblock ".len()..].parse().unwrap();
            let want = if case.kinds.contains(&DamageKind::Checksum) {
                Integrity::Failed
            } else {
                Integrity::Verified
            };
            assert_eq!(
                report.superblocks.copies[ag].integrity, want,
                "{}",
                case.tag
            );
        }
    }
}
