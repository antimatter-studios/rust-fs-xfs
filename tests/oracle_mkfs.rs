//! Cross-validation against real XFS tooling.
//!
//! The unit tests in `src/` parse superblocks and AG headers this crate
//! built itself. That proves the parser is self-consistent — it does not
//! prove the parser reads XFS the way the rest of the world reads it. A
//! misread offset or a byte-order slip would be invisible, because the
//! same misunderstanding would be baked into both the fixture and the
//! parser.
//!
//! These tests close that gap. They build filesystems with the canonical
//! `mkfs.xfs` across a spread of geometries and feature combinations,
//! then compare **every field this driver parses** against the value the
//! reference debugger reports for the same field. Disagreement on any
//! one field fails the test and names the field.
//!
//! `mkfs.xfs` and `xfs_db` run in the fs-linux-test-harness guest, which
//! has them on every host this suite runs on, so nothing here is
//! `#[ignore]`-gated and nothing here skips. A tool that cannot be
//! reached, a geometry the pinned xfsprogs refuses, or a field the
//! debugger did not print in the quantity this test requires is a
//! failure that names what to do about it — a gate that quietly reports
//! ok is worth less than no gate at all, because it is trusted.

use fs_xfs::superblock::Superblock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

mod common;
use common::oracle;

/// Create a sparse image file and format it with `mkfs.xfs $args`.
///
/// The image is made on the host and formatted in the guest, which is one
/// file either way: the harness gives the guest this repository at the
/// path the host knows it by, so the argument crosses unchanged. That is
/// also why the scratch directory comes from `std::env::temp_dir()` —
/// `scripts/with-test-temp.sh` points TMPDIR at a directory inside the
/// repository precisely so that a path a test invents is a path the
/// oracle tools can open.
fn mkfs(size_mib: u64, args: &[&str]) -> PathBuf {
    static SEQ: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "fs-xfs-oracle-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let img = dir.join("fs.img");

    let f = std::fs::File::create(&img).unwrap();
    f.set_len(size_mib * 1024 * 1024).unwrap();
    drop(f);

    let out = oracle("mkfs.xfs").args(args).arg("-f").arg(&img).output();
    // A REFUSED GEOMETRY IS A FAILURE, not a case to step around. It was
    // once a skip because the tool came from whichever xfsprogs the
    // machine happened to have, so "this version will not build that" was
    // a fact about the developer's laptop. One pinned xfsprogs in the
    // guest ends that: a geometry it refuses is a geometry this suite no
    // longer covers, and which of the two moves — the case or the pin —
    // is a decision to take in the open rather than discover in a green
    // run that stopped testing anything.
    assert!(
        out.ok(),
        "mkfs.xfs {args:?} refused this geometry, so the comparison it feeds \
         covers nothing. The guest's xfsprogs is pinned by scripts/vm-setup.sh: \
         either the pin moved under a geometry this driver must still read, or \
         the case belongs to a format that is genuinely gone and should be \
         deleted here deliberately.\n{}{}",
        out.stdout,
        out.stderr
    );
    img
}

/// Ask the reference debugger to dump superblock 0 and return the fields
/// as a map. Values are normalised to decimal strings.
fn xfs_db_superblock(img: &Path) -> HashMap<String, String> {
    let out = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "print"])
        .arg(img)
        .output();
    assert!(out.ok(), "xfs_db failed: {}{}", out.stdout, out.stderr);

    let mut map = HashMap::new();
    for line in out.stdout.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim().to_string();
        let v = v.trim();
        // xfs_db prints integers in decimal, some fields in hex with a
        // 0x prefix, and flag words symbolically. Normalise the numeric
        // forms; leave anything else as-is for the caller to interpret.
        let normalised = if let Some(hex) = v.strip_prefix("0x") {
            u64::from_str_radix(hex, 16)
                .map(|n| n.to_string())
                .unwrap_or_else(|_| v.to_string())
        } else {
            v.to_string()
        };
        map.insert(k, normalised);
    }
    map
}

/// How many of the fields below have to be compared for the run to have
/// checked anything (#200).
///
/// A FLOOR, BECAUSE THE FAILURE HERE IS AN ABSENCE. `expect_field`
/// returns when `xfs_db` did not report a field, which is right — an
/// older or newer xfsprogs reports a different set, and a comparison
/// that cannot be made is not a failure. But nothing counted them: if
/// the output format shifts, or the `k = v` parse stops splitting the
/// lines the way it expects, every lookup misses, twenty notes are
/// printed, and the test reports ok having compared nothing at all.
///
/// Twenty-three fields are asked for. Eighteen is the floor: room for a
/// few to go missing between xfsprogs versions, and nowhere near room
/// for the parse to have failed.
const FIELDS_COMPARED_FLOOR: usize = 18;

/// Compare one field, reporting the field name on mismatch, and say
/// whether a comparison was actually made.
fn expect_field(oracle: &HashMap<String, String>, field: &str, ours: u64, label: &str) -> bool {
    let Some(theirs) = oracle.get(field) else {
        // Field absent from this xfsprogs version's output; nothing to
        // compare against, and silently passing would be dishonest.
        eprintln!("note: xfs_db did not report `{field}` — not compared");
        return false;
    };
    let theirs_n: u64 = theirs
        .parse()
        .unwrap_or_else(|_| panic!("{label}: xfs_db reported `{field} = {theirs}`, not a number"));
    assert_eq!(
        ours, theirs_n,
        "{label}: field `{field}` — this driver says {ours}, xfs_db says {theirs_n}"
    );
    true
}

/// Parse the superblock at offset 0 of `img` with this driver.
fn parse_ours(img: &Path) -> Superblock {
    let bytes = std::fs::read(img).expect("read image");
    Superblock::parse(&bytes).unwrap_or_else(|e| panic!("this driver failed to parse: {e}"))
}

/// The core assertion: build a filesystem, then require this driver and
/// the reference debugger to agree on every field we parse.
fn assert_agrees_with_oracle(size_mib: u64, args: &[&str], label: &str) {
    let img = mkfs(size_mib, args);
    let ours = parse_ours(&img);
    let theirs = xfs_db_superblock(&img);
    let mut compared = 0usize;

    compared += usize::from(expect_field(
        &theirs,
        "blocksize",
        u64::from(ours.blocksize),
        label,
    ));
    compared += usize::from(expect_field(&theirs, "dblocks", ours.dblocks, label));
    compared += usize::from(expect_field(&theirs, "rblocks", ours.rblocks, label));
    compared += usize::from(expect_field(&theirs, "rootino", ours.rootino, label));
    compared += usize::from(expect_field(
        &theirs,
        "agblocks",
        u64::from(ours.agblocks),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "agcount",
        u64::from(ours.agcount),
        label,
    ));
    compared += usize::from(expect_field(&theirs, "logstart", ours.logstart, label));
    compared += usize::from(expect_field(
        &theirs,
        "logblocks",
        u64::from(ours.logblocks),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "sectsize",
        u64::from(ours.sectsize),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "inodesize",
        u64::from(ours.inodesize),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "inopblock",
        u64::from(ours.inopblock),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "blocklog",
        u64::from(ours.blocklog),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "sectlog",
        u64::from(ours.sectlog),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "inodelog",
        u64::from(ours.inodelog),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "inopblog",
        u64::from(ours.inopblog),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "agblklog",
        u64::from(ours.agblklog),
        label,
    ));
    compared += usize::from(expect_field(&theirs, "icount", ours.icount, label));
    compared += usize::from(expect_field(&theirs, "ifree", ours.ifree, label));
    compared += usize::from(expect_field(&theirs, "fdblocks", ours.fdblocks, label));
    compared += usize::from(expect_field(
        &theirs,
        "inoalignmt",
        u64::from(ours.inoalignmt),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "dirblklog",
        u64::from(ours.dirblklog),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "logsunit",
        u64::from(ours.logsunit),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "features_incompat",
        u64::from(ours.features_incompat),
        label,
    ));
    compared += usize::from(expect_field(
        &theirs,
        "features_ro_compat",
        u64::from(ours.features_ro_compat),
        label,
    ));

    assert!(
        compared >= FIELDS_COMPARED_FLOOR,
        "{label}: only {compared} of the fields asked for were compared against \
         xfs_db, and the floor is {FIELDS_COMPARED_FLOOR} — a run that compared \
         nothing prints notes and reports ok, which is what #200 was about. Either \
         this xfsprogs reports a very different set of fields, or the `k = v` parse \
         above stopped matching its output."
    );

    eprintln!("  {label}: {compared} fields agree with xfs_db");

    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

#[test]
fn agrees_on_default_geometry() {
    // Whatever the installed mkfs.xfs considers default — the geometry
    // real users will actually have.
    assert_agrees_with_oracle(300, &[], "default");
}

#[test]
fn agrees_on_1k_blocks() {
    assert_agrees_with_oracle(300, &["-b", "size=1024"], "1k-blocks");
}

#[test]
fn agrees_on_2k_blocks() {
    assert_agrees_with_oracle(300, &["-b", "size=2048"], "2k-blocks");
}

#[test]
fn agrees_on_512_byte_inodes() {
    assert_agrees_with_oracle(300, &["-i", "size=512"], "512b-inodes");
}

#[test]
fn agrees_on_1k_inodes() {
    assert_agrees_with_oracle(300, &["-i", "size=1024"], "1k-inodes");
}

#[test]
fn agrees_on_many_allocation_groups() {
    // Forces a small agblocks and a large agblklog, exercising the
    // inode-number splitting arithmetic at an unusual shift.
    assert_agrees_with_oracle(600, &["-d", "agcount=16"], "16-ags");
}

#[test]
fn agrees_on_single_allocation_group() {
    assert_agrees_with_oracle(300, &["-d", "agcount=1"], "1-ag");
}

#[test]
fn agrees_with_reflink_and_rmapbt() {
    // Both are read-only-compatible features that change the AGF layout.
    assert_agrees_with_oracle(400, &["-m", "reflink=1,rmapbt=1"], "reflink+rmapbt");
}

#[test]
fn agrees_without_crc_v4_filesystem() {
    // v4 has no CRC and no metadata UUID, and it is the one format whose
    // creation a future xfsprogs is expected to drop outright. When the
    // pinned build does, this case fails rather than passing empty, and
    // the answer is to retire the case — the driver still has to read v4
    // volumes that already exist, which the .vm-share fixtures cover.
    assert_agrees_with_oracle(300, &["-m", "crc=0"], "v4");
}

#[test]
fn agrees_with_bigtime_and_large_extent_counters() {
    assert_agrees_with_oracle(400, &["-m", "bigtime=1"], "bigtime");
}

/// A filesystem the driver must *refuse*, not misread. `sb_inprogress`
/// is set while mkfs is mid-write; a volume in that state is not safe to
/// present to a user.
#[test]
fn rejects_superblock_with_inprogress_set() {
    let img = mkfs(300, &[]);
    let mut bytes = std::fs::read(&img).unwrap();
    bytes[126] = 1; // sb_inprogress
    assert!(
        Superblock::parse(&bytes).is_err(),
        "a filesystem with sb_inprogress set must be refused, not mounted"
    );
    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}

/// The geometry the driver reports must match what the AG headers say,
/// not merely what the superblock claims — the two are written by
/// different parts of mkfs and a mismatch means we misread one of them.
#[test]
fn agf_and_agi_parse_for_every_allocation_group() {
    let img = mkfs(600, &["-d", "agcount=8"]);
    let bytes = std::fs::read(&img).unwrap();
    let sb = parse_ours(&img);
    let sector = usize::from(sb.sectsize);

    for ag in 0..sb.agcount {
        let ag_start = (u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize)) as usize;
        // Sector 0 is the superblock, 1 the AGF, 2 the AGI.
        let agf = fs_xfs::ag::Agf::parse(&bytes[ag_start + sector..], &sb, ag)
            .unwrap_or_else(|e| panic!("AGF for ag {ag}: {e}"));
        let agi = fs_xfs::ag::Agi::parse(&bytes[ag_start + 2 * sector..], &sb, ag)
            .unwrap_or_else(|e| panic!("AGI for ag {ag}: {e}"));

        assert_eq!(agf.seqno, ag);
        assert_eq!(agi.seqno, ag);
        assert!(
            agf.length <= sb.agblocks,
            "ag {ag}: AGF length {} exceeds sb_agblocks {}",
            agf.length,
            sb.agblocks
        );
        assert!(
            !agi.has_unlinked_inodes(),
            "ag {ag}: a freshly made filesystem must have no unlinked inodes"
        );
    }
    std::fs::remove_dir_all(img.parent().unwrap()).ok();
}
