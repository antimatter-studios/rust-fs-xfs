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
//! What it does NOT prove is that `Superblock` models every field. It
//! does not: the realtime inodes, the quota inodes, the stripe geometry
//! and several others are carried across untouched rather than
//! understood. `apply` is written to preserve them precisely so this
//! test can hold while that remains true. See the module documentation
//! for why building from nothing needs them first.
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
