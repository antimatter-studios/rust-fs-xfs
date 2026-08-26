//! Freeing an extent must do to the free-space trees exactly what the
//! kernel's own truncate does.
//!
//! A file truncated to zero looks the same whether its blocks went back
//! to the allocation group correctly, went back twice, or did not go
//! back at all. The difference is entirely in the two free-space trees
//! and the group header, so the result cannot check itself.
//!
//! So the oracle is a **pair** of images: the same filesystem before and
//! after the kernel truncated a file. This reads the free space of the
//! first, frees the victim's extents into it, and requires the result to
//! equal the free space of the second — every record, in both trees,
//! plus the two totals the group header carries.
//!
//! Predicting the second image from the first is a much stronger claim
//! than producing trees that merely look well-formed. There is exactly
//! one right answer and the kernel has already written it down.
//!
//! # The four cases
//!
//! What the freed extent adjoins is what decides the outcome, so the
//! fixtures cover all four and the script picks the neighbour to remove
//! by measuring where files actually landed:
//!
//! | fixture | the victim's blocks adjoin | outcome |
//! |---|---|---|
//! | `lone` | nothing | a new record |
//! | `after` | free space after them | a record grows downwards |
//! | `before` | free space before them | a record grows upwards |
//! | `between` | free space on both sides | two records become one |
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-truncate-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agf;
use fs_xfs::alloc_btree::{
    free_extent, longest, total_free, walk_from_agf, FreeExtent, Freed, Order,
};
use fs_xfs::error::Result;
use fs_xfs::superblock::Superblock;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The file each fixture truncates.
const VICTIM: &str = "/victim";

/// Each fixture, and what freeing the victim's extent should turn out
/// to be. Stating the expected outcome rather than only comparing the
/// trees is what stops a fixture that quietly stopped covering its case
/// from still passing — which is how an earlier version of the fixture
/// script built the same case four times.
const CASES: &[(&str, Freed)] = &[
    ("lone", Freed::Inserted),
    ("after", Freed::MergedWithFollowing),
    ("before", Freed::MergedWithPreceding),
    ("between", Freed::JoinedTwo),
];

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// One allocation group's free space, as both trees hold it, and the
/// two totals its header states.
struct GroupFreeSpace {
    by_block: Vec<FreeExtent>,
    by_count: Vec<FreeExtent>,
    freeblks: u32,
    longest: u32,
}

fn read_group(dev: &FileDevice, sb: &Superblock, agno: u32) -> GroupFreeSpace {
    let block = u64::from(sb.blocksize);
    let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;

    let mut raw = vec![0u8; usize::from(sb.sectsize)];
    dev.read_at(ag_start + u64::from(sb.sectsize), &mut raw)
        .expect("read the group header");
    let agf = Agf::parse(&raw, sb, agno).expect("parse the group header");

    let read = |agblock: u32| -> Result<Vec<u8>> {
        let mut buf = vec![0u8; sb.blocksize as usize];
        dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
        Ok(buf)
    };

    GroupFreeSpace {
        by_block: walk_from_agf(sb, &agf, Order::ByBlock, read).expect("bnobt"),
        by_count: walk_from_agf(sb, &agf, Order::ByCount, read).expect("cntbt"),
        freeblks: agf.freeblks,
        longest: agf.longest,
    }
}

/// Which allocation group a filesystem block belongs to, and where it
/// sits inside it.
fn split_fsblock(sb: &Superblock, fsblock: u64) -> (u32, u32) {
    let agno = (fsblock >> sb.agblklog) as u32;
    let agblock = (fsblock & ((1u64 << sb.agblklog) - 1)) as u32;
    (agno, agblock)
}

/// The victim's extents, from the image where it still has some.
fn victim_extents(path: &Path) -> Vec<(u32, FreeExtent)> {
    let dev = Arc::new(FileDevice::open(path).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    let found = fs.lookup_path(VICTIM).expect("the victim is in the image");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("read the victim");
    let sb = fs.superblock();

    fs.data_extents(&inode, &raw)
        .expect("the victim's extents")
        .iter()
        .map(|e| {
            let (agno, agblock) = split_fsblock(sb, e.startblock);
            (
                agno,
                FreeExtent {
                    startblock: agblock,
                    blockcount: e.blockcount as u32,
                },
            )
        })
        .collect()
}

/// Free the victim's blocks into the before-image's trees and require
/// the result to be the after-image's trees.
#[test]
fn freeing_an_extent_reproduces_what_the_kernel_did() {
    let mut ran = 0usize;

    for &(case, expected) in CASES {
        let before_path = share().join(format!("xfstrunc-{case}-before.img"));
        let after_path = share().join(format!("xfstrunc-{case}-after.img"));
        if !before_path.exists() || !after_path.exists() {
            continue;
        }

        let before_dev = FileDevice::open(&before_path).expect("open before");
        let mut sbb = vec![0u8; 4096];
        before_dev.read_at(0, &mut sbb).expect("read");
        let sb = Superblock::parse(&sbb).expect("superblock");

        let after_dev = FileDevice::open(&after_path).expect("open after");

        let extents = victim_extents(&before_path);
        assert!(
            !extents.is_empty(),
            "{case}: the victim has no extents in the before image, \
             so the fixture is not testing anything"
        );

        // Only the groups the victim had blocks in can change, and this
        // checks every group either way — a driver that freed blocks
        // into the wrong group would otherwise pass.
        for agno in 0..sb.agcount {
            let before = read_group(&before_dev, &sb, agno);
            let after = read_group(&after_dev, &sb, agno);

            let mut predicted = before.by_block.clone();
            let mut outcomes = Vec::new();
            for (owner, extent) in extents.iter().filter(|(owner, _)| *owner == agno) {
                let outcome = free_extent(&mut predicted, *extent)
                    .unwrap_or_else(|e| panic!("{case} AG {owner}: {e}"));
                outcomes.push(outcome);
            }

            if outcomes.is_empty() {
                assert_eq!(
                    before.by_block, after.by_block,
                    "{case} AG {agno}: the victim had no blocks here, \
                     so nothing should have changed"
                );
                continue;
            }

            assert!(
                outcomes.contains(&expected),
                "{case} AG {agno}: freeing gave {outcomes:?}, and the fixture is \
                 supposed to exercise {expected:?} — the fixture has stopped \
                 covering the case it names"
            );

            assert_eq!(
                predicted, after.by_block,
                "{case} AG {agno}: the free space after freeing does not match \
                 what the kernel produced.\n predicted: {predicted:?}\n kernel:    {:?}\n \
                 (started from {:?}, freed {:?})",
                after.by_block, before.by_block, extents
            );

            // The group header's two totals, recomputed rather than
            // carried over — they are what an allocator reads before it
            // looks at a tree at all.
            assert_eq!(
                total_free(&predicted),
                u64::from(after.freeblks),
                "{case} AG {agno}: predicted {} free blocks, the kernel wrote {}",
                total_free(&predicted),
                after.freeblks
            );
            assert_eq!(
                longest(&predicted),
                after.longest,
                "{case} AG {agno}: predicted a longest run of {}, the kernel wrote {}",
                longest(&predicted),
                after.longest
            );

            // The second tree holds the same extents, so predicting one
            // predicts the other. Checking it is what catches a merge
            // that produced the right blocks in the wrong number of
            // records.
            let mut sorted = predicted.clone();
            sorted.sort_by_key(|e| (e.blockcount, e.startblock));
            assert_eq!(
                sorted, after.by_count,
                "{case} AG {agno}: the by-length tree disagrees with the prediction"
            );
        }
        ran += 1;
    }

    if ran == 0 {
        eprintln!(
            "no truncate fixtures in {}; build them with \
             ./scripts/vm-build-truncate-fixtures.sh",
            share().display()
        );
        return;
    }
    assert_eq!(
        ran,
        CASES.len(),
        "only {ran} of the {} cases were present, so some outcome went unchecked",
        CASES.len()
    );
    eprintln!("all {ran} truncate cases match what the kernel wrote");
}
