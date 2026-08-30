//! Writing a superblock back must reproduce what `mkfs.xfs` wrote.
//!
//! Every fixture under `.vm-share` is a real filesystem the canonical
//! `mkfs.xfs` produced. Parsing one and applying it back over its own
//! bytes must change nothing: the buffer afterwards is identical to the
//! buffer before, checksum included.
//!
//! That is a stronger claim than it looks. It fails if any offset in the
//! table is wrong, if any field is written at the wrong width, if the
//! byte order is inverted anywhere, or if the CRC covers the wrong span
//! — and it fails on ten different geometries, chosen to move the fields
//! most likely to be misread. It is the check the rest of a formatter
//! will rest on, because a formatter that cannot reproduce a superblock
//! it can read has no chance of building one from nothing.
//!
//! On its own it would not prove that `Superblock` models every field —
//! a field nobody parsed would keep its original value simply because
//! nothing overwrote it. That is what
//! `applying_into_an_empty_buffer_reproduces_the_original` is for: the
//! same comparison against a destination that starts as zeroes, where
//! an unmodelled field reads back as zero and the assertion names its
//! offset. The two together say the offset table is right *and* the
//! model is complete, which is the pair a formatter needs.
//!
//! Fixtures are gitignored. Build them with `chore fixtures`.

use fs_xfs::super_write::apply;
use fs_xfs::superblock::Superblock;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every captured image, by name.
fn fixtures() -> Vec<(String, Vec<u8>)> {
    let dir = share();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let name = p.file_stem().unwrap().to_string_lossy().to_string();
        // Only the geometry fixtures are plain formatted images; the
        // data/dirconv ones have been mounted and written to, which is
        // fine here but adds nothing — the superblock is what is under
        // test and these ten already span the interesting geometries.
        if !name.starts_with("xfs-") {
            continue;
        }
        // Read one sector's worth generously: the largest sector size
        // these fixtures use is 4096, and `parse` only looks at the
        // superblock.
        let Ok(bytes) = read_prefix(&p, 4096) else {
            continue;
        };
        out.push((name, bytes));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn read_prefix(path: &Path, n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Parse, write back, and require the bytes to be unchanged.
#[test]
fn applying_a_parsed_superblock_reproduces_it_byte_for_byte() {
    let all = fixtures();
    if all.is_empty() {
        eprintln!("no fixtures in .vm-share; build them with `chore fixtures`");
        return;
    }

    let mut checked = 0usize;
    for (name, original) in &all {
        let Ok(sb) = Superblock::parse(original) else {
            // A fixture this crate refuses to parse is a separate
            // problem, and the parser's own tests own it.
            eprintln!("{name}: does not parse, skipped");
            continue;
        };

        let mut ours = original.clone();
        apply(&mut ours, &sb).expect("apply a superblock over itself");

        let sector = sb.sectsize as usize;
        let differing: Vec<usize> = (0..sector).filter(|&i| ours[i] != original[i]).collect();

        assert!(
            differing.is_empty(),
            "{name}: {} byte(s) differ after a round trip, first at {:#06x} \
             (wrote {:#04x}, mkfs.xfs wrote {:#04x}). That names a field whose \
             offset, width or byte order is wrong.",
            differing.len(),
            differing.first().copied().unwrap_or(0),
            ours[differing.first().copied().unwrap_or(0)],
            original[differing.first().copied().unwrap_or(0)]
        );
        checked += 1;
    }

    eprintln!("{checked} superblocks reproduced byte for byte");
    assert!(
        checked >= 5,
        "only {checked} fixtures checked — too few to span the geometries \
         that move the log2 fields and the AG layout"
    );
}

/// The round trip is not vacuous: a changed field must change the bytes.
///
/// Without this, an `apply` that did nothing at all would pass the test
/// above on every fixture.
#[test]
fn the_round_trip_would_notice_a_changed_field() {
    let all = fixtures();
    if all.is_empty() {
        eprintln!("no fixtures in .vm-share; build them with `chore fixtures`");
        return;
    }
    let (name, original) = &all[0];
    let Ok(mut sb) = Superblock::parse(original) else {
        eprintln!("{name}: does not parse, skipped");
        return;
    };

    // `icount` is a plain counter — safe to move without making the
    // superblock structurally invalid, and it is one `validate` does not
    // constrain.
    sb.icount = sb.icount.wrapping_add(1);

    let mut ours = original.clone();
    apply(&mut ours, &sb).expect("apply");
    assert_ne!(
        &ours[..sb.sectsize as usize],
        &original[..sb.sectsize as usize],
        "{name}: changing icount left the bytes identical, so the round-trip \
         test above proves nothing"
    );
}

/// The checksum is recomputed, not copied.
///
/// A v5 superblock whose CRC is stale is one the kernel refuses, so an
/// `apply` that preserved the old checksum would produce an unmountable
/// filesystem while passing a comparison against the original.
#[test]
fn the_checksum_is_recomputed_from_the_bytes() {
    let all = fixtures();
    if all.is_empty() {
        eprintln!("no fixtures in .vm-share; build them with `chore fixtures`");
        return;
    }

    let mut checked = 0usize;
    for (name, original) in &all {
        let Ok(sb) = Superblock::parse(original) else {
            continue;
        };
        if !sb.is_v5() {
            continue; // v4 has no CRC to recompute.
        }

        // Blank the checksum first, so a match cannot come from having
        // left mkfs.xfs's own answer in place.
        let mut ours = original.clone();
        ours[224..228].fill(0);
        apply(&mut ours, &sb).expect("apply");

        assert_eq!(
            &ours[224..228],
            &original[224..228],
            "{name}: the recomputed CRC does not match the one mkfs.xfs wrote"
        );
        checked += 1;
    }
    eprintln!("{checked} checksums recomputed correctly");
    assert!(checked >= 4, "only {checked} v5 fixtures checked");
}

/// The model is complete: applying into zeroes rebuilds the superblock.
///
/// This is the test the round trip above cannot be: there, a field
/// `Superblock` does not model keeps whatever `mkfs.xfs` put in it,
/// because nothing overwrites it. Here the destination starts empty, so
/// an unmodelled field reads back as zero and the comparison names the
/// offset it belongs at.
///
/// It is also the shape a formatter uses. A formatter has no superblock
/// to carry fields across from — it has a zeroed sector and a geometry,
/// and `apply` has to be able to fill the whole structure from the
/// struct alone.
#[test]
fn applying_into_an_empty_buffer_reproduces_the_original() {
    let all = fixtures();
    if all.is_empty() {
        eprintln!("no fixtures in .vm-share; build them with `chore fixtures`");
        return;
    }

    let mut checked = 0usize;
    for (name, original) in &all {
        let Ok(sb) = Superblock::parse(original) else {
            eprintln!("{name}: does not parse, skipped");
            continue;
        };

        let sector = sb.sectsize as usize;
        // Nothing of the original is carried over: the destination is
        // as empty as the disk a formatter is handed.
        let mut ours = vec![0u8; sector];
        apply(&mut ours, &sb).expect("apply into an empty sector");

        let differing: Vec<usize> = (0..sector).filter(|&i| ours[i] != original[i]).collect();
        assert!(
            differing.is_empty(),
            "{name}: {} byte(s) differ when built from the parsed struct alone, \
             first at {:#06x} (wrote {:#04x}, mkfs.xfs wrote {:#04x}). That names \
             a field `Superblock` does not model, or models at the wrong offset.",
            differing.len(),
            differing.first().copied().unwrap_or(0),
            ours[differing.first().copied().unwrap_or(0)],
            original[differing.first().copied().unwrap_or(0)]
        );
        checked += 1;
    }

    eprintln!("{checked} superblocks rebuilt from the struct alone");
    assert!(
        checked >= 5,
        "only {checked} fixtures checked — too few to span the geometries"
    );
}

/// A v4 superblock does not gain v5 fields.
///
/// `sb_pquotino` and `sb_lsn` live past the 208-byte v4 structure. On a
/// v4 filesystem those bytes are not those fields, and writing them
/// there would produce a superblock that differs from the one `mkfs.xfs
/// -m crc=0` wrote — which the test above would catch, but only while a
/// v4 fixture is in the set. This says so directly.
#[test]
fn a_v4_superblock_is_written_without_the_v5_extension() {
    let all = fixtures();
    let Some((name, original)) = all.iter().find(|(_, b)| {
        // version lives in the low nibble of sb_versionnum, at 100.
        b.len() > 102 && (u16::from_be_bytes([b[100], b[101]]) & 0x000f) == 4
    }) else {
        eprintln!("no v4 fixture in .vm-share; build them with `chore fixtures`");
        return;
    };

    let sb = Superblock::parse(original).expect("parse the v4 fixture");
    assert!(!sb.is_v5(), "{name} is not v4 after all");

    let mut ours = vec![0u8; sb.sectsize as usize];
    apply(&mut ours, &sb).expect("apply into an empty sector");

    // Everything from the end of the v4 structure to the end of the
    // sector must still be untouched.
    assert!(
        ours[208..].iter().all(|&b| b == 0),
        "{name}: writing a v4 superblock put bytes past the 208-byte \
         structure, where sb_features_compat and the v5 extension would be"
    );
}

/// Every carried field reaches its own offset.
///
/// Ten of the fields added to model the whole structure — `rextents`,
/// `rbmblocks`, `rextslog`, `frextents`, the quota inodes, `qflags`,
/// `flags`, `shared_vn`, the stripe geometry, the log sector geometry,
/// `pquotino`, `lsn` — are **zero in every fixture `mkfs.xfs` produces
/// with default options**. So the byte-for-byte tests above cannot tell
/// a correct write of one from no write at all: zero in, zero out,
/// matching either way.
///
/// This gives each a distinct value and reads it back at the offset the
/// format assigns it. Distinct, because two fields sharing a sentinel
/// would let a transposition pass.
#[test]
fn each_carried_field_lands_at_its_own_offset() {
    use fs_xfs::superblock::offsets;

    let all = fixtures();
    let Some((name, original)) = all
        .iter()
        .find(|(_, b)| b.len() > 102 && (u16::from_be_bytes([b[100], b[101]]) & 0x000f) == 5)
    else {
        eprintln!("no v5 fixture in .vm-share; build them with `chore fixtures`");
        return;
    };

    let mut sb = Superblock::parse(original).expect("parse the v5 fixture");

    // Distinct so a transposition cannot pass, and none of them is a
    // value `validate` constrains — validation already ran, above.
    sb.rextents = 0x1122_3344_5566_7701;
    sb.rbmino = 0x1122_3344_5566_7702;
    sb.rsumino = 0x1122_3344_5566_7703;
    sb.rextsize = 0x1122_3304;
    sb.rbmblocks = 0x1122_3305;
    sb.rextslog = 0x06;
    sb.imax_pct = 0x07;
    sb.frextents = 0x1122_3344_5566_7708;
    sb.uquotino = 0x1122_3344_5566_7709;
    sb.gquotino = 0x1122_3344_5566_770a;
    sb.qflags = 0x110b;
    sb.flags = 0x0c;
    sb.shared_vn = 0x0d;
    sb.unit = 0x1122_330e;
    sb.width = 0x1122_330f;
    sb.logsectlog = 0x10;
    sb.logsectsize = 0x1111;
    sb.bad_features2 = 0x1122_3312;
    sb.pquotino = 0x1122_3344_5566_7713;
    sb.lsn = 0x1122_3344_5566_7714;

    let mut buf = vec![0u8; sb.sectsize as usize];
    apply(&mut buf, &sb).expect("apply into an empty sector");

    let be64 = |at: usize| u64::from_be_bytes(buf[at..at + 8].try_into().unwrap());
    let be32 = |at: usize| u32::from_be_bytes(buf[at..at + 4].try_into().unwrap());
    let be16 = |at: usize| u16::from_be_bytes(buf[at..at + 2].try_into().unwrap());

    let checks: Vec<(&str, u64, u64)> = vec![
        ("sb_rextents", be64(offsets::REXTENTS), sb.rextents),
        ("sb_rbmino", be64(offsets::RBMINO), sb.rbmino),
        ("sb_rsumino", be64(offsets::RSUMINO), sb.rsumino),
        (
            "sb_rextsize",
            be32(offsets::REXTSIZE).into(),
            sb.rextsize.into(),
        ),
        (
            "sb_rbmblocks",
            be32(offsets::RBMBLOCKS).into(),
            sb.rbmblocks.into(),
        ),
        (
            "sb_rextslog",
            buf[offsets::REXTSLOG].into(),
            sb.rextslog.into(),
        ),
        (
            "sb_imax_pct",
            buf[offsets::IMAX_PCT].into(),
            sb.imax_pct.into(),
        ),
        ("sb_frextents", be64(offsets::FREXTENTS), sb.frextents),
        ("sb_uquotino", be64(offsets::UQUOTINO), sb.uquotino),
        ("sb_gquotino", be64(offsets::GQUOTINO), sb.gquotino),
        ("sb_qflags", be16(offsets::QFLAGS).into(), sb.qflags.into()),
        ("sb_flags", buf[offsets::FLAGS].into(), sb.flags.into()),
        (
            "sb_shared_vn",
            buf[offsets::SHARED_VN].into(),
            sb.shared_vn.into(),
        ),
        ("sb_unit", be32(offsets::UNIT).into(), sb.unit.into()),
        ("sb_width", be32(offsets::WIDTH).into(), sb.width.into()),
        (
            "sb_logsectlog",
            buf[offsets::LOGSECTLOG].into(),
            sb.logsectlog.into(),
        ),
        (
            "sb_logsectsize",
            be16(offsets::LOGSECTSIZE).into(),
            sb.logsectsize.into(),
        ),
        (
            "sb_bad_features2",
            be32(offsets::BAD_FEATURES2).into(),
            sb.bad_features2.into(),
        ),
        ("sb_pquotino", be64(offsets::PQUOTINO), sb.pquotino),
        ("sb_lsn", be64(offsets::LSN), sb.lsn as u64),
    ];
    for (field, on_disk, expected) in checks {
        assert_eq!(
            on_disk, expected,
            "{name}: {field} did not reach its offset (found {on_disk:#x}, \
             set {expected:#x})"
        );
    }

    // And the whole thing still parses, which says the values did not
    // land somewhere that breaks a field the reader depends on.
    let back = Superblock::parse(&buf).expect("the rewritten superblock parses");
    assert_eq!(back.rextents, sb.rextents);
    assert_eq!(back.lsn, sb.lsn);
    assert_eq!(back.bad_features2, sb.bad_features2);
}
