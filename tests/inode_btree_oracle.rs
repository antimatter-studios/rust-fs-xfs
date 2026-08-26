//! The inode trees this driver reads must agree with the ones the kernel
//! wrote — with the group's own header, and with each other.
//!
//! An allocation group states how many inodes it holds and how many are
//! free, and then keeps the chunks themselves in a B+tree. A v5
//! filesystem keeps a second tree holding only the chunks that still
//! have a free inode. Three independent statements of the same facts,
//! all written by XFS, is what makes a reader checkable without a
//! running kernel.
//!
//! Required of every allocation group of every fixture:
//!
//! - the chunks' inode counts sum to `agi_count`;
//! - their free counts sum to `agi_freecount`;
//! - each chunk's free **bitmap** has exactly as many bits set as its
//!   own free count says — the count and the bitmap are stored
//!   separately and a reader that had the record layout wrong would
//!   disagree with itself here;
//! - the chunks are ordered by starting inode and none overlaps another;
//! - and the free-inode tree holds **exactly** the chunks with a free
//!   inode in them, no more and no fewer.
//!
//! The last is the strongest. The two trees are written independently
//! and hold different subsets, so a reader that misread the record would
//! have to misread it into two consistent halves for this to pass.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agi;
use fs_xfs::error::Result;
use fs_xfs::inode_btree::{walk_from_agi, InodeChunk, Which, INODES_PER_CHUNK};
use fs_xfs::superblock::Superblock;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every allocation group of one image, checked. Returns how many groups
/// were examined and whether the image had a free-inode tree.
fn check_image(path: &Path) -> Option<(usize, bool)> {
    let dev = FileDevice::open(path).ok()?;
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).ok()?;
    let sb = Superblock::parse(&sbb).ok()?;
    let name = path.file_name().unwrap().to_string_lossy().into_owned();

    let block = u64::from(sb.blocksize);
    let mut had_finobt = false;

    for agno in 0..sb.agcount {
        let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;

        // The inode header is the group's third sector.
        let mut raw = vec![0u8; usize::from(sb.sectsize)];
        dev.read_at(ag_start + 2 * u64::from(sb.sectsize), &mut raw)
            .expect("read the group inode header");
        let agi = Agi::parse(&raw, &sb, agno).expect("parse the group inode header");

        let read = |agblock: u32| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; sb.blocksize as usize];
            dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
            Ok(buf)
        };

        let all = walk_from_agi(&sb, &agi, Which::All, read)
            .unwrap_or_else(|e| panic!("{name} AG {agno}: inobt: {e}"))
            .expect("every filesystem has an inode tree");

        // The header's totals, recomputed from the chunks.
        let inodes: u64 = all.iter().map(|c| u64::from(c.count)).sum();
        assert_eq!(
            inodes,
            u64::from(agi.count),
            "{name} AG {agno}: the chunks hold {inodes} inodes, the header says {}",
            agi.count
        );

        let free: u64 = all.iter().map(|c| u64::from(c.freecount)).sum();
        assert_eq!(
            free,
            u64::from(agi.freecount),
            "{name} AG {agno}: the chunks have {free} inodes free, the header says {}",
            agi.freecount
        );

        let mut previous_end = 0u64;
        for chunk in &all {
            // The count and the bitmap are stored separately, so their
            // agreeing is evidence about the record layout and not just
            // about the arithmetic above.
            assert_eq!(
                chunk.free.count_ones(),
                u32::from(chunk.freecount),
                "{name} AG {agno}: chunk at {} has {} bits set in its free map and a free \
                 count of {}",
                chunk.startino,
                chunk.free.count_ones(),
                chunk.freecount
            );
            assert!(
                chunk.count <= INODES_PER_CHUNK,
                "{name} AG {agno}: chunk at {} claims {} inodes, more than a chunk holds",
                chunk.startino,
                chunk.count
            );
            assert!(
                chunk.freecount <= chunk.count,
                "{name} AG {agno}: chunk at {} has {} of {} inodes free",
                chunk.startino,
                chunk.freecount,
                chunk.count
            );

            let start = u64::from(chunk.startino);
            assert!(
                start >= previous_end,
                "{name} AG {agno}: chunk at {start} overlaps the one before it, or the \
                 tree is out of order"
            );
            previous_end = start + u64::from(INODES_PER_CHUNK);
        }

        // The free-inode tree holds exactly the chunks with something
        // free in them.
        let with_free = walk_from_agi(&sb, &agi, Which::WithFreeInodes, read)
            .unwrap_or_else(|e| panic!("{name} AG {agno}: finobt: {e}"));
        if let Some(with_free) = with_free {
            had_finobt = true;

            let expected: BTreeSet<u32> = all
                .iter()
                .filter(|c| c.freecount > 0)
                .map(|c| c.startino)
                .collect();
            let found: BTreeSet<u32> = with_free.iter().map(|c| c.startino).collect();
            assert_eq!(
                found, expected,
                "{name} AG {agno}: the free-inode tree does not hold exactly the chunks \
                 with a free inode"
            );

            // And they must be the same chunks, not merely the same
            // starting inodes.
            let by_start: Vec<&InodeChunk> = all.iter().filter(|c| c.freecount > 0).collect();
            for (a, b) in by_start.iter().zip(&with_free) {
                assert_eq!(
                    a.free, b.free,
                    "{name} AG {agno}: the two trees disagree about which inodes of the \
                     chunk at {} are free",
                    a.startino
                );
            }
        }
    }

    Some((sb.agcount as usize, had_finobt))
}

/// Every fixture, every allocation group.
#[test]
fn every_group_of_every_fixture_reconciles() {
    let mut images = 0usize;
    let mut groups = 0usize;
    let mut with_finobt = 0usize;

    let Ok(entries) = std::fs::read_dir(share()) else {
        eprintln!("no fixtures in {}", share().display());
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "img"))
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with("xfs"))
        })
        .collect();
    paths.sort();

    for path in paths {
        if let Some((n, finobt)) = check_image(&path) {
            images += 1;
            groups += n;
            if finobt {
                with_finobt += 1;
            }
        }
    }

    if images == 0 {
        eprintln!("no XFS fixtures found; build them with ./scripts/vm-build-fixtures.sh");
        return;
    }
    eprintln!(
        "{groups} allocation groups across {images} images reconcile \
         ({with_finobt} of them with a free-inode tree)"
    );
    assert!(groups >= images);
}

/// Both record shapes must actually be exercised, or the distinction
/// between them is only asserted in a unit test and never met.
///
/// The `nosparse` fixture is a v5 filesystem built with `-i sparse=0`;
/// without it, every v5 image would take the packed branch and a reader
/// that only ever took that branch would pass everything.
#[test]
fn both_record_shapes_are_covered() {
    let mut sparse = 0usize;
    let mut plain = 0usize;

    let Ok(entries) = std::fs::read_dir(share()) else {
        return;
    };
    for path in entries.flatten().map(|e| e.path()) {
        if path.extension().is_none_or(|e| e != "img") {
            continue;
        }
        let Ok(dev) = FileDevice::open(&path) else {
            continue;
        };
        let mut sbb = vec![0u8; 4096];
        if dev.read_at(0, &mut sbb).is_err() {
            continue;
        }
        let Ok(sb) = Superblock::parse(&sbb) else {
            continue;
        };
        if sb.has_sparse_inodes() {
            sparse += 1;
        } else {
            plain += 1;
        }
    }

    if sparse + plain == 0 {
        eprintln!("no fixtures; nothing to cover");
        return;
    }
    eprintln!("{sparse} fixtures use the packed record, {plain} the plain one");
    assert!(
        sparse > 0,
        "no fixture uses sparse inodes, so the packed record is never read"
    );
    assert!(
        plain > 0,
        "no fixture has sparse inodes off, so the plain record is never read — build \
         them with ./scripts/vm-build-fixtures.sh, which includes a v5 image made with \
         -i sparse=0 for exactly this reason"
    );
}
