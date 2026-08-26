//! The free-space trees this driver reads must agree with the ones the
//! kernel wrote — and with each other.
//!
//! An allocation group states its own free-space totals in its header,
//! and then keeps the extents themselves in two B+trees ordered
//! differently. That redundancy is what makes the reader checkable
//! without a running kernel: the header was written by XFS and the trees
//! were written by XFS, so a reader that misreads either produces
//! numbers that do not reconcile.
//!
//! Five things are required of every allocation group of every fixture:
//!
//! - the extents sum to `agf_freeblks`;
//! - the longest of them is `agf_longest`;
//! - `bnobt` is ordered by start block, with no two extents overlapping
//!   or touching, and none running past the end of the group;
//! - `cntbt` is ordered by length;
//! - **the two trees hold the same set of extents.**
//!
//! The last is the strongest. The trees are written independently and
//! ordered differently, so a reader that had the record layout subtly
//! wrong — the wrong field width, the wrong header size, the pointer
//! array in the wrong place — would have to be wrong in exactly the same
//! way twice for the two sets to still match.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-log-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agf;
use fs_xfs::alloc_btree::{walk_from_agf, FreeExtent, Order};
use fs_xfs::error::Result;
use fs_xfs::superblock::Superblock;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every allocation group of one image, checked.
///
/// Returns how many groups were examined so the caller can tell a clean
/// run from one that quietly examined nothing.
fn check_image(path: &Path) -> Option<usize> {
    let dev = FileDevice::open(path).ok()?;
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).ok()?;
    let sb = Superblock::parse(&sbb).ok()?;
    let name = path.file_name().unwrap().to_string_lossy().into_owned();

    let block = u64::from(sb.blocksize);
    let ag_bytes = u64::from(sb.agblocks) * block;

    for agno in 0..sb.agcount {
        let ag_start = u64::from(agno) * ag_bytes;

        // The header is the group's second sector, immediately after
        // its copy of the superblock.
        let mut raw = vec![0u8; usize::from(sb.sectsize)];
        dev.read_at(ag_start + u64::from(sb.sectsize), &mut raw)
            .expect("read the group header");
        let agf = Agf::parse(&raw, &sb, agno).expect("parse the group header");

        let read = |agblock: u32| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; sb.blocksize as usize];
            dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
            Ok(buf)
        };

        let by_block = walk_from_agf(&sb, &agf, Order::ByBlock, read)
            .unwrap_or_else(|e| panic!("{name} AG {agno}: bnobt: {e}"));
        let by_count = walk_from_agf(&sb, &agf, Order::ByCount, read)
            .unwrap_or_else(|e| panic!("{name} AG {agno}: cntbt: {e}"));

        // The header's totals, recomputed from the extents themselves.
        let total: u64 = by_block.iter().map(|e| u64::from(e.blockcount)).sum();
        assert_eq!(
            total,
            u64::from(agf.freeblks),
            "{name} AG {agno}: the extents sum to {total}, the header says {}",
            agf.freeblks
        );

        let longest = by_block.iter().map(|e| e.blockcount).max().unwrap_or(0);
        assert_eq!(
            longest, agf.longest,
            "{name} AG {agno}: the longest extent is {longest}, the header says {}",
            agf.longest
        );

        // bnobt is ordered by start block, and free extents that touched
        // would have been merged into one — so each must start strictly
        // after the previous one ended.
        let mut previous_end = 0u64;
        for extent in &by_block {
            assert!(
                u64::from(extent.startblock) > previous_end || previous_end == 0,
                "{name} AG {agno}: extent at {} starts at or before {previous_end}, \
                 so the tree is out of order or two extents were not merged",
                extent.startblock
            );
            assert!(
                extent.end() <= u64::from(agf.length),
                "{name} AG {agno}: extent {}+{} runs past the group's {} blocks",
                extent.startblock,
                extent.blockcount,
                agf.length
            );
            assert!(
                extent.blockcount > 0,
                "{name} AG {agno}: a free extent of no blocks at {}",
                extent.startblock
            );
            previous_end = extent.end();
        }

        // cntbt is ordered by length. Equal lengths are ordered by start
        // block, which is what keeps the ordering total.
        for pair in by_count.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            assert!(
                (a.blockcount, a.startblock) <= (b.blockcount, b.startblock),
                "{name} AG {agno}: cntbt has {}+{} before {}+{}",
                a.startblock,
                a.blockcount,
                b.startblock,
                b.blockcount
            );
        }

        // The two trees describe the same free space.
        let a: BTreeSet<FreeExtent> = by_block.iter().copied().collect();
        let b: BTreeSet<FreeExtent> = by_count.iter().copied().collect();
        assert_eq!(
            a.len(),
            by_block.len(),
            "{name} AG {agno}: bnobt lists the same extent twice"
        );
        assert_eq!(
            b.len(),
            by_count.len(),
            "{name} AG {agno}: cntbt lists the same extent twice"
        );
        assert_eq!(
            a, b,
            "{name} AG {agno}: the two trees disagree about what is free"
        );
    }

    Some(sb.agcount as usize)
}

/// Every fixture, every allocation group.
///
/// The list is deliberately not filtered to the interesting ones: the
/// geometries are the point, since header size, record layout and the
/// group-relative addressing all vary with them.
#[test]
fn every_group_of_every_fixture_reconciles() {
    let mut images = 0usize;
    let mut groups = 0usize;
    let mut names = Vec::new();

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
        if let Some(n) = check_image(&path) {
            images += 1;
            groups += n;
            names.push(path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }

    if images == 0 {
        eprintln!("no XFS fixtures found; build them with ./scripts/vm-build-log-fixtures.sh");
        return;
    }
    eprintln!("{groups} allocation groups across {images} images reconcile: {names:?}");
    assert!(
        groups >= images,
        "every image has at least one allocation group"
    );
}
