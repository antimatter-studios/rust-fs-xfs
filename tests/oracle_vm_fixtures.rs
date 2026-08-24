//! Cross-validation against fixtures built by real XFS tooling in the VM.
//!
//! `mkfs.xfs` and `xfs_db` are Linux-only, so on macOS the fixtures are
//! produced inside the oracle VM (`scripts/vm.sh`) and left in
//! `.vm-share`: each `xfs-<name>.img` paired with an `xfs-<name>.sbdump`
//! holding `xfs_db -c 'sb 0' -c print` output for that image.
//!
//! This test then parses the image **on the host** and requires every
//! field it reports to match the value the reference debugger reports for
//! the same field. That is the difference between proving the parser is
//! self-consistent and proving it reads XFS the way the rest of the world
//! does — a byte-order slip or a misread offset survives the former and
//! dies here.
//!
//! Running the comparison on the host keeps the iterate-and-check loop
//! fast: the VM is only needed when fixtures are regenerated, not on
//! every `cargo test`.
//!
//! Fixtures are gitignored and absent on a fresh clone, so this test
//! skips rather than fails when `.vm-share` is empty. Generate them with:
//!
//! ```sh
//! ./scripts/vm.sh up
//! ./scripts/vm-build-fixtures.sh
//! ```

use fs_xfs::superblock::Superblock;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Parse an `xfs_db ... print` dump into a field map, normalising the
/// numeric forms the debugger uses (plain decimal, or `0x`-prefixed hex).
fn parse_sbdump(text: &str) -> HashMap<String, u64> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let k = k.trim();
        let v = v.trim();
        let parsed = if let Some(hex) = v.strip_prefix("0x") {
            u64::from_str_radix(hex, 16).ok()
        } else {
            v.parse::<u64>().ok()
        };
        if let Some(n) = parsed {
            map.insert(k.to_string(), n);
        }
    }
    map
}

/// Locate every `.img` in `.vm-share` that has a matching `.sbdump`.
fn fixtures() -> Vec<(String, PathBuf, PathBuf)> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let Ok(entries) = std::fs::read_dir(&share) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let dump = p.with_extension("sbdump");
        if dump.exists() {
            let name = p.file_stem().unwrap().to_string_lossy().into_owned();
            out.push((name, p, dump));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Compare one field, naming it and both readings on mismatch.
fn expect(oracle: &HashMap<String, u64>, field: &str, ours: u64, label: &str, checked: &mut usize) {
    let Some(&theirs) = oracle.get(field) else {
        // Field not reported by this xfsprogs version. Say so rather than
        // passing silently — an unnoticed skip is a hole in the gate.
        eprintln!("  {label}: xfs_db did not report `{field}` — not compared");
        return;
    };
    assert_eq!(
        ours, theirs,
        "{label}: field `{field}` — this driver says {ours}, xfs_db says {theirs}"
    );
    *checked += 1;
}

#[test]
fn agrees_with_xfs_db_on_every_field() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures in .vm-share — run ./scripts/vm-build-fixtures.sh; skipping");
        return;
    }

    let mut total_fields = 0usize;
    for (label, img, dump) in &fixtures {
        let bytes = std::fs::read(img).expect("read image");
        let sb = Superblock::parse(&bytes)
            .unwrap_or_else(|e| panic!("{label}: this driver failed to parse a real image: {e}"));
        let oracle = parse_sbdump(&std::fs::read_to_string(dump).expect("read sbdump"));
        assert!(
            !oracle.is_empty(),
            "{label}: sbdump held no parseable fields"
        );

        let mut checked = 0usize;
        expect(
            &oracle,
            "blocksize",
            sb.blocksize.into(),
            label,
            &mut checked,
        );
        expect(&oracle, "dblocks", sb.dblocks, label, &mut checked);
        expect(&oracle, "rblocks", sb.rblocks, label, &mut checked);
        expect(&oracle, "rootino", sb.rootino, label, &mut checked);
        expect(&oracle, "agblocks", sb.agblocks.into(), label, &mut checked);
        expect(&oracle, "agcount", sb.agcount.into(), label, &mut checked);
        expect(&oracle, "logstart", sb.logstart, label, &mut checked);
        expect(
            &oracle,
            "logblocks",
            sb.logblocks.into(),
            label,
            &mut checked,
        );
        expect(&oracle, "sectsize", sb.sectsize.into(), label, &mut checked);
        expect(
            &oracle,
            "inodesize",
            sb.inodesize.into(),
            label,
            &mut checked,
        );
        expect(
            &oracle,
            "inopblock",
            sb.inopblock.into(),
            label,
            &mut checked,
        );
        expect(&oracle, "blocklog", sb.blocklog.into(), label, &mut checked);
        expect(&oracle, "sectlog", sb.sectlog.into(), label, &mut checked);
        expect(&oracle, "inodelog", sb.inodelog.into(), label, &mut checked);
        expect(&oracle, "inopblog", sb.inopblog.into(), label, &mut checked);
        expect(&oracle, "agblklog", sb.agblklog.into(), label, &mut checked);
        expect(&oracle, "icount", sb.icount, label, &mut checked);
        expect(&oracle, "ifree", sb.ifree, label, &mut checked);
        expect(&oracle, "fdblocks", sb.fdblocks, label, &mut checked);
        expect(
            &oracle,
            "inoalignmt",
            sb.inoalignmt.into(),
            label,
            &mut checked,
        );
        expect(
            &oracle,
            "dirblklog",
            sb.dirblklog.into(),
            label,
            &mut checked,
        );
        expect(&oracle, "logsunit", sb.logsunit.into(), label, &mut checked);
        expect(
            &oracle,
            "spino_align",
            sb.spino_align.into(),
            label,
            &mut checked,
        );
        expect(
            &oracle,
            "features_incompat",
            sb.features_incompat.into(),
            label,
            &mut checked,
        );
        expect(
            &oracle,
            "features_ro_compat",
            sb.features_ro_compat.into(),
            label,
            &mut checked,
        );
        expect(
            &oracle,
            "features_compat",
            sb.features_compat.into(),
            label,
            &mut checked,
        );

        assert!(
            checked >= 15,
            "{label}: only {checked} fields could be compared — the oracle dump \
             is not providing enough coverage to call this validated"
        );
        eprintln!("  {label}: {checked} fields agree with xfs_db");
        total_fields += checked;
    }
    eprintln!(
        "{} fixtures, {total_fields} field comparisons against xfs_db",
        fixtures.len()
    );
}

/// Every allocation group's AGF and AGI must parse and self-identify, on
/// images a real mkfs produced. This exercises the v5 identity checks
/// against genuine metadata rather than fixtures we built ourselves.
#[test]
fn ag_headers_parse_on_real_images() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures in .vm-share — skipping");
        return;
    }

    for (label, img, _) in &fixtures {
        let bytes = std::fs::read(img).expect("read image");
        let sb = Superblock::parse(&bytes).expect("parse superblock");
        let sector = usize::from(sb.sectsize);

        for ag in 0..sb.agcount {
            let base = (u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize)) as usize;
            // Sector 0 is the superblock, sector 1 the AGF, sector 2 the AGI.
            let agf = fs_xfs::ag::Agf::parse(&bytes[base + sector..], &sb, ag)
                .unwrap_or_else(|e| panic!("{label} ag {ag}: AGF: {e}"));
            let agi = fs_xfs::ag::Agi::parse(&bytes[base + 2 * sector..], &sb, ag)
                .unwrap_or_else(|e| panic!("{label} ag {ag}: AGI: {e}"));

            assert_eq!(agf.seqno, ag, "{label}: AGF {ag} self-identifies wrongly");
            assert_eq!(agi.seqno, ag, "{label}: AGI {ag} self-identifies wrongly");
            assert!(
                agf.length <= sb.agblocks,
                "{label} ag {ag}: AGF length {} exceeds sb_agblocks {}",
                agf.length,
                sb.agblocks
            );
            assert!(
                agf.freeblks <= agf.length,
                "{label} ag {ag}: more free blocks than blocks"
            );
            assert!(
                !agi.has_unlinked_inodes(),
                "{label} ag {ag}: freshly made filesystem has unlinked inodes"
            );
        }
        eprintln!("  {label}: {} AGs verified", sb.agcount);
    }
}

/// Reading an AG header at the wrong AG index must be rejected. Run
/// against real metadata, this proves the identity check does real work
/// rather than passing because our own fixtures happened to agree.
#[test]
fn real_ag_headers_reject_wrong_index() {
    let fixtures = fixtures();
    let Some((label, img, _)) = fixtures.iter().find(|(_, p, _)| {
        std::fs::read(p)
            .ok()
            .and_then(|b| Superblock::parse(&b).ok())
            .map(|sb| sb.agcount > 1)
            .unwrap_or(false)
    }) else {
        eprintln!("no multi-AG fixture available — skipping");
        return;
    };

    let bytes = std::fs::read(img).unwrap();
    let sb = Superblock::parse(&bytes).unwrap();
    let sector = usize::from(sb.sectsize);
    // AG 0's AGF, deliberately parsed as though it came from AG 1.
    let res = fs_xfs::ag::Agf::parse(&bytes[sector..], &sb, 1);
    assert!(
        matches!(res, Err(fs_xfs::Error::BlockIdentityMismatch { .. })),
        "{label}: AG 0's AGF was accepted as AG 1's — the identity check is not working"
    );
}
