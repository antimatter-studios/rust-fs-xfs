//! Taking an inode must do to the inode trees exactly what the kernel's
//! own create does.
//!
//! A file that exists looks the same whether the inode it was given was
//! properly taken out of its group's accounting or merely written over.
//! The difference is in the two inode trees and the group header — so,
//! as with freeing an extent, the result cannot check itself and the
//! oracle is a **pair** of images: the same filesystem before and after
//! the kernel created a file.
//!
//! # The three cases, and why the fill levels matter
//!
//! Inodes come in chunks of 64, and what happens depends on how full the
//! chunk in use is:
//!
//! | fixture | before | outcome |
//! |---|---|---|
//! | `spare` | 55 files | a chunk with room; it keeps its place in the free-inode tree |
//! | `last` | 60 files | the chunk's **last** free inode; it leaves the free-inode tree |
//! | `newchunk` | 61 files | nothing free anywhere; a whole new chunk is allocated |
//!
//! A fixture that only ever creates a file on a fresh filesystem
//! exercises the first and gives no sign the others exist. The third is
//! refused by name rather than attempted, since allocating a chunk
//! allocates blocks too — and the test asserts that it is refused, so
//! the refusal cannot quietly become a wrong answer.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-create-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agi;
use fs_xfs::error::Result;
use fs_xfs::inode_btree::{choose_free_inode, walk_from_agi, InodeChunk, Taken, Which};
use fs_xfs::superblock::Superblock;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// A group's inode chunks, the chunks its free-inode tree holds, and the
/// totals its header states.
struct GroupInodes {
    all: Vec<InodeChunk>,
    with_free: Vec<InodeChunk>,
    count: u32,
    freecount: u32,
}

fn read_group(dev: &FileDevice, sb: &Superblock, agno: u32) -> GroupInodes {
    let block = u64::from(sb.blocksize);
    let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;

    let mut raw = vec![0u8; usize::from(sb.sectsize)];
    dev.read_at(ag_start + 2 * u64::from(sb.sectsize), &mut raw)
        .expect("read the group inode header");
    let agi = Agi::parse(&raw, sb, agno).expect("parse the group inode header");

    let read = |agblock: u32| -> Result<Vec<u8>> {
        let mut buf = vec![0u8; sb.blocksize as usize];
        dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
        Ok(buf)
    };

    GroupInodes {
        all: walk_from_agi(sb, &agi, Which::All, read)
            .expect("inobt")
            .unwrap_or_default(),
        with_free: walk_from_agi(sb, &agi, Which::WithFreeInodes, read)
            .expect("finobt")
            .unwrap_or_default(),
        count: agi.count,
        freecount: agi.freecount,
    }
}

/// The group a create's new inode landed in — the one whose free count
/// went down, or whose chunk list grew.
fn changed_group(
    before: &FileDevice,
    after: &FileDevice,
    sb: &Superblock,
) -> Option<(u32, GroupInodes, GroupInodes)> {
    (0..sb.agcount).find_map(|agno| {
        let b = read_group(before, sb, agno);
        let a = read_group(after, sb, agno);
        (b.freecount != a.freecount || b.count != a.count).then_some((agno, b, a))
    })
}

/// Taking an inode reproduces what the kernel's create did.
#[test]
fn taking_an_inode_reproduces_what_the_kernel_did() {
    let mut ran = Vec::new();

    for (case, expected) in [
        ("spare", Some(Taken::ChunkStillHasFree)),
        ("last", Some(Taken::ChunkNowFull)),
        // Nothing free anywhere: a create has to allocate a whole chunk,
        // which this refuses.
        ("newchunk", None),
    ] {
        let before_path = share().join(format!("xfscreate-{case}-before.img"));
        let after_path = share().join(format!("xfscreate-{case}-after.img"));
        if !before_path.exists() || !after_path.exists() {
            continue;
        }

        let before_dev = FileDevice::open(&before_path).expect("open before");
        let mut sbb = vec![0u8; 4096];
        before_dev.read_at(0, &mut sbb).expect("read");
        let sb = Superblock::parse(&sbb).expect("superblock");
        let after_dev = FileDevice::open(&after_path).expect("open after");

        let (agno, before, after) = changed_group(&before_dev, &after_dev, &sb)
            .unwrap_or_else(|| panic!("{case}: no group changed, so the fixture created nothing"));

        let Some(expected) = expected else {
            // The refusal case. The kernel grew the group a whole chunk;
            // this must decline rather than invent one.
            assert!(
                choose_free_inode(&before.all).is_none(),
                "{case}: a free inode was found where the kernel had to allocate a new \
                 chunk — the fixture is no longer exercising the case it names"
            );
            assert!(
                after.count > before.count,
                "{case}: the kernel should have grown the group by a chunk"
            );
            ran.push(case);
            continue;
        };

        let (index, n) = choose_free_inode(&before.all)
            .unwrap_or_else(|| panic!("{case}: no free inode, but the kernel found one"));

        let mut predicted = before.all.clone();
        let outcome = predicted[index]
            .take(n)
            .unwrap_or_else(|e| panic!("{case}: {e}"));

        assert_eq!(
            outcome, expected,
            "{case} AG {agno}: taking an inode gave {outcome:?}, and the fixture is \
             supposed to exercise {expected:?}"
        );

        // The chunk itself: the right inode, and only that one.
        assert_eq!(
            predicted, after.all,
            "{case} AG {agno}: the inode chunks after taking one do not match what the \
             kernel produced.\n predicted: {predicted:?}\n kernel:    {:?}",
            after.all
        );

        // The header's totals, recomputed.
        let free: u32 = predicted.iter().map(|c| u32::from(c.freecount)).sum();
        assert_eq!(
            free, after.freecount,
            "{case} AG {agno}: predicted {free} free inodes, the kernel wrote {}",
            after.freecount
        );
        assert_eq!(
            before.count, after.count,
            "{case} AG {agno}: the group should not have gained a chunk"
        );

        // And the free-inode tree's membership, which is what the
        // outcome above is really about.
        let expected_members: BTreeSet<u32> = predicted
            .iter()
            .filter(|c| c.freecount > 0)
            .map(|c| c.startino)
            .collect();
        let actual_members: BTreeSet<u32> = after.with_free.iter().map(|c| c.startino).collect();
        assert_eq!(
            expected_members, actual_members,
            "{case} AG {agno}: the free-inode tree does not hold the chunks it should \
             after the create"
        );

        ran.push(case);
    }

    if ran.is_empty() {
        eprintln!("no create fixtures; build them with ./scripts/vm-build-create-fixtures.sh");
        return;
    }
    assert_eq!(
        ran.len(),
        3,
        "only {ran:?} of the three cases were present, so an outcome went unchecked"
    );
    eprintln!("all three create cases match what the kernel wrote");
}

/// Taking an inode and giving it back leaves the chunk as it was, and
/// the two enumerations line up.
///
/// Weaker than matching the kernel, but it covers arrangements no
/// fixture holds — and it is the property that matters once unlink
/// exists alongside create.
#[test]
fn taking_then_giving_back_leaves_no_trace() {
    use fs_xfs::inode_btree::Given;

    let arrangements: &[(u64, u8)] = &[
        // A fresh chunk: three in use, sixty-one free.
        (0xffff_ffff_ffff_fff8, 61),
        // One free inode left.
        (0x8000_0000_0000_0000, 1),
        // Half of them.
        (0x0000_0000_ffff_ffff, 32),
    ];

    for &(free, freecount) in arrangements {
        let start = InodeChunk {
            startino: 128,
            holemask: 0,
            count: 64,
            freecount,
            free,
        };

        let n = start.first_free().expect("something is free");
        let mut chunk = start;
        let taken = chunk.take(n).expect("take it");
        let given = chunk.give_back(n).expect("give it back");

        assert_eq!(
            chunk, start,
            "taking then giving back changed the chunk: {taken:?} then {given:?}"
        );
        let expected = match taken {
            Taken::ChunkNowFull => Given::ChunkWasFull,
            Taken::ChunkStillHasFree => Given::ChunkAlreadyHadFree,
        };
        assert_eq!(
            given, expected,
            "{taken:?} should be undone by {expected:?}, not {given:?}"
        );
    }
}
