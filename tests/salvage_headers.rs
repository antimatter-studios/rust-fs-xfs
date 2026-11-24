//! The salvage reads of superblocks and group headers, on images
//! `mkfs.xfs` and the kernel made, and on damaged copies of them.
//!
//! The oracle is `xfs_db`: each fixture's `.sbdump` is what it printed for
//! the primary superblock when the fixture was built, and the dirty-log
//! fixture's `.verdict` is what `xfs_repair -n` said of its log. The damage
//! here is made by inverting bytes whose meaning the format fixes, so the
//! finding each one must produce does not depend on this crate's reading.
//! `tests/salvage_headers_oracle.rs` makes damage with `xfs_db` itself and
//! asks `xfs_repair -n` to agree.

mod common;

use common::{fixture, fixtures_matching, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, DamageKind, Integrity, Severity};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

const SUITE: &str = "salvage_headers";

/// `field = value` lines of an `xfs_db` dump, numbers only.
fn sbdump(path: &Path) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(" = ")?;
            Some((key.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

fn number(dump: &HashMap<String, String>, field: &str) -> u64 {
    let raw = dump
        .get(field)
        .unwrap_or_else(|| panic!("the dump has no {field}"));
    match raw.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
        None => raw.parse().unwrap(),
    }
}

fn uuid_text(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The geometry fixtures: every `xfs-*.img` with an `.sbdump` beside it.
fn geometry_fixtures() -> Vec<(String, std::path::PathBuf)> {
    let out: Vec<_> = fixtures_matching("xfs-", ".img")
        .into_iter()
        .filter(|p| p.with_extension("sbdump").is_file())
        .map(|p| (p.file_stem().unwrap().to_string_lossy().into_owned(), p))
        .collect();
    assert!(
        !out.is_empty(),
        "no xfs-*.img fixture has an .sbdump beside it: `chore fixtures` builds them"
    );
    out
}

fn open(path: &Path) -> FileDevice {
    FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display()))
}

/// Invert the bytes at `at`.
fn invert(path: &Path, at: u64, length: usize) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut bytes = vec![0u8; length];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.read_exact(&mut bytes).unwrap();
    for b in &mut bytes {
        *b = !*b;
    }
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&bytes).unwrap();
    f.sync_all().unwrap();
}

fn damaged_copy(source: &str, tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &fixture(source),
        &format!("{}-{tag}.img", std::process::id()),
    )
}

/// On every geometry fixture, the chosen geometry is what `xfs_db` printed,
/// there is one superblock per group, each passes the check its version
/// has, and nothing anywhere is reported damaged.
#[test]
fn undamaged_fixtures_agree_with_xfs_db_and_report_nothing() {
    for (name, image) in geometry_fixtures() {
        let dump = sbdump(&image.with_extension("sbdump"));
        let device = open(&image);
        let report = salvage::damage(&device);
        let sb = report
            .superblocks
            .geometry
            .as_ref()
            .unwrap_or_else(|| panic!("{name}: no geometry"));
        for (field, ours) in [
            ("blocksize", u64::from(sb.blocksize)),
            ("dblocks", sb.dblocks),
            ("agblocks", u64::from(sb.agblocks)),
            ("agcount", u64::from(sb.agcount)),
            ("logstart", sb.logstart),
            ("logblocks", u64::from(sb.logblocks)),
            ("sectsize", u64::from(sb.sectsize)),
            ("inodesize", u64::from(sb.inodesize)),
            ("rootino", sb.rootino),
        ] {
            assert_eq!(ours, number(&dump, field), "{name}: {field}");
        }
        assert_eq!(uuid_text(&sb.uuid), dump["uuid"], "{name}: uuid");
        assert_eq!(report.superblocks.source, Some(0), "{name}: source");
        assert_eq!(
            report.superblocks.copies.len() as u64,
            number(&dump, "agcount"),
            "{name}: one superblock per group"
        );
        let v5 = number(&dump, "versionnum") & 0xf == 5;
        for copy in &report.superblocks.copies {
            let want = if v5 {
                Integrity::Verified
            } else {
                Integrity::Unchecked
            };
            assert_eq!(copy.integrity, want, "{name}: superblock {:?}", copy.ag);
        }
        assert_eq!(report.groups.len() as u64, number(&dump, "agcount"));
        let all: Vec<String> = report.all().map(|d| format!("{d:?}")).collect();
        assert!(all.is_empty(), "{name}: {all:#?}");
    }
}

/// The salvage read of each group header decodes what the strict parser
/// does, on every fixture.
#[test]
fn group_headers_decode_as_the_strict_parsers_do() {
    for (name, image) in geometry_fixtures() {
        let device = std::sync::Arc::new(open(&image));
        let fs = fs_xfs::Filesystem::mount(device.clone()).unwrap();
        let sb = fs.superblock().clone();
        for ag in 0..sb.agcount {
            let headers = salvage::ag_headers(device.as_ref(), &sb, ag);
            assert_eq!(
                headers.agf.value.as_ref(),
                Some(&fs.read_agf(ag).unwrap()),
                "{name}: AGF {ag}"
            );
            assert_eq!(
                headers.agi.value.as_ref(),
                Some(&fs.read_agi(ag).unwrap()),
                "{name}: AGI {ag}"
            );
            let want = if sb.is_v5() {
                Integrity::Verified
            } else {
                Integrity::Unchecked
            };
            assert_eq!(headers.agf.integrity, want, "{name}: AGF {ag}");
            assert_eq!(headers.agi.integrity, want, "{name}: AGI {ag}");
            assert_eq!(headers.agfl.integrity, want, "{name}: AGFL {ag}");
        }
    }
}

/// A primary superblock whose checksum fails is reported, still decoded,
/// and the geometry comes from a copy that passes, agreeing with `xfs_db`.
#[test]
fn a_primary_that_fails_its_checksum_is_read_and_outvoted() {
    let copy = damaged_copy("xfs-default.img", "primary-crc");
    // Byte 400 is in the sector's zero padding past the 264-byte
    // structure: covered by the checksum, part of no field.
    invert(copy.path(), 400, 1);
    let dump = sbdump(&fixture("xfs-default.sbdump"));
    let found = salvage::superblocks(&open(copy.path()));
    let primary = &found.copies[0];
    assert_eq!(primary.integrity, Integrity::Failed);
    assert!(primary.superblock.is_some(), "the fields are still decoded");
    assert_eq!(primary.damage.len(), 1, "{:#?}", primary.damage);
    assert_eq!(primary.damage[0].kind, DamageKind::Checksum);
    assert_eq!(primary.damage[0].severity, Severity::Error);
    assert_eq!(primary.damage[0].location.offset, 0);
    assert_eq!(found.source, Some(1), "a verified copy is preferred");
    let sb = found.geometry.unwrap();
    assert_eq!(u64::from(sb.agblocks), number(&dump, "agblocks"));
    assert_eq!(sb.dblocks, number(&dump, "dblocks"));
}

/// With the primary's sector gone, the copies are found by scanning, and
/// the geometry is still the one `xfs_db` printed.
#[test]
fn a_destroyed_primary_is_replaced_by_a_copy_found_by_scanning() {
    let copy = damaged_copy("xfs-default.img", "primary-gone");
    invert(copy.path(), 0, 512);
    let dump = sbdump(&fixture("xfs-default.sbdump"));
    let report = salvage::damage(&open(copy.path()));
    let found = &report.superblocks;
    assert!(found.copies[0].superblock.is_none());
    assert_eq!(found.copies[0].damage[0].kind, DamageKind::Signature);
    assert_eq!(found.copies.len() as u64, number(&dump, "agcount"));
    assert_eq!(found.source, Some(1));
    let sb = found.geometry.as_ref().unwrap();
    assert_eq!(u64::from(sb.agblocks), number(&dump, "agblocks"));
    assert_eq!(u64::from(sb.agcount), number(&dump, "agcount"));
    assert_eq!(uuid_text(&sb.uuid), dump["uuid"]);
    // The primary's sector is the only damage: the group headers and the
    // log are found and clean.
    let all: Vec<_> = report.all().collect();
    assert_eq!(all.len(), 1, "{all:#?}");
}

/// A copy whose geometry was changed and re-checksummed is a disagreement,
/// not a checksum failure, and does not move the volume's geometry.
#[test]
fn a_copy_that_disagrees_is_reported_against_the_geometry() {
    let copy = damaged_copy("xfs-default.img", "copy-agblocks");
    let dump = sbdump(&fixture("xfs-default.sbdump"));
    let sb = fs_xfs::Superblock::parse(&std::fs::read(fixture("xfs-default.img")).unwrap()[..512])
        .unwrap();
    let at = sb.ag_offset(2);
    // sb_logblocks of copy 2 changed and its checksum
    // re-stamped, so only the comparison with the other copies can find it.
    let mut sector = vec![0u8; 512];
    {
        let mut f = std::fs::File::open(copy.path()).unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut sector).unwrap();
    }
    let field = fs_xfs::superblock::offsets::LOGBLOCKS;
    let logblocks = u32::from_be_bytes(sector[field..field + 4].try_into().unwrap());
    sector[field..field + 4].copy_from_slice(&(logblocks - 1).to_be_bytes());
    fs_xfs::super_write::stamp_crc(&mut sector);
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .open(copy.path())
            .unwrap();
        f.seek(SeekFrom::Start(at)).unwrap();
        f.write_all(&sector).unwrap();
    }
    let found = salvage::superblocks(&open(copy.path()));
    assert_eq!(found.source, Some(0));
    assert_eq!(
        u64::from(found.geometry.as_ref().unwrap().logblocks),
        number(&dump, "logblocks")
    );
    assert_eq!(found.copies[2].integrity, Integrity::Verified);
    assert_eq!(found.damage.len(), 1, "{:#?}", found.damage);
    assert_eq!(found.damage[0].kind, DamageKind::Mismatch);
    assert_eq!(found.damage[0].structure, "superblock 2");
    assert_eq!(found.damage[0].severity, Severity::Warning);
}

/// Damage in group headers is found, named by group, and the headers'
/// fields are still decoded wherever the magic says the sector is theirs.
#[test]
fn damaged_group_headers_are_reported_and_still_decoded() {
    let copy = damaged_copy("xfs-default.img", "headers");
    let image = std::fs::read(fixture("xfs-default.img")).unwrap();
    let sb = fs_xfs::Superblock::parse(&image[..512]).unwrap();
    let sector = u64::from(sb.sectsize);
    // AGF 1: its magic.
    invert(copy.path(), sb.ag_offset(1) + sector, 1);
    // AGI 2: the low byte of agi_count (offset 16), which leaves the
    // checksum wrong and every field readable and in range.
    invert(copy.path(), sb.ag_offset(2) + 2 * sector + 19, 1);
    let device = open(copy.path());

    let one = salvage::ag_headers(&device, &sb, 1);
    assert!(one.agf.value.is_none());
    assert_eq!(one.agf.damage.len(), 1);
    assert_eq!(one.agf.damage[0].kind, DamageKind::Signature);
    assert_eq!(one.agf.damage[0].structure, "AGF 1");
    assert_eq!(one.agf.damage[0].location.offset, sb.ag_offset(1) + sector);

    let two = salvage::ag_headers(&device, &sb, 2);
    assert_eq!(two.agi.integrity, Integrity::Failed);
    let agi = two.agi.value.as_ref().expect("decoded past the checksum");
    assert_eq!(agi.seqno, 2);
    assert!(two
        .agi
        .damage
        .iter()
        .any(|d| d.kind == DamageKind::Checksum && d.structure == "AGI 2"));

    let report = salvage::damage(&device);
    let structures: Vec<&str> = report.all().map(|d| d.structure.as_str()).collect();
    assert!(structures.contains(&"AGF 1"), "{structures:?}");
    assert!(structures.contains(&"AGI 2"), "{structures:?}");
    assert!(
        structures.iter().all(|s| *s == "AGF 1" || *s == "AGI 2"),
        "{structures:?}"
    );
}

/// The log of the fixture `xfs_repair -n` called dirty is reported as
/// needing replay, as a notice; the source is not written.
#[test]
fn a_log_that_needs_replay_is_a_notice() {
    let verdict = std::fs::read_to_string(fixture("xfsdirty.verdict")).unwrap();
    assert_eq!(
        verdict.trim(),
        "DIRTY",
        "xfs_repair -n's verdict on the fixture"
    );
    let image = fixture("xfsdirty.img");
    let before = std::fs::read(&image).unwrap();
    let report = salvage::damage(&open(&image));
    let log: Vec<_> = report.log.iter().collect();
    assert_eq!(log.len(), 1, "{log:#?}");
    assert_eq!(log[0].kind, DamageKind::Recorded);
    assert_eq!(log[0].severity, Severity::Notice);
    assert_eq!(
        std::fs::read(&image).unwrap(),
        before,
        "nothing was written"
    );
}

/// A device that is not XFS at all has no geometry and no copies beyond
/// the primary's sector, which is reported unrecognised.
#[test]
fn zeros_are_not_a_volume() {
    let volume =
        scratch::Volume::empty(SUITE, &format!("{}-zeros.img", std::process::id()), 1 << 22);
    let report = salvage::damage(&open(volume.path()));
    assert!(report.superblocks.geometry.is_none());
    assert_eq!(report.superblocks.copies.len(), 1);
    assert!(report.groups.is_empty());
}
