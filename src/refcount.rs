//! The reference-count B+tree: which extents more than one file holds.
//!
//! `reflink` lets two files point at the same blocks. The refcount tree
//! is what stops the second one being surprised when the first is
//! deleted: it holds a record per shared extent saying how many owners
//! it has, and blocks go back to free space only when the last one lets
//! go.
//!
//! # Why a driver that writes has to care
//!
//! Freeing returns blocks to the group's free space. If another file
//! still points at them, the allocator will hand them out again and the
//! two files will overwrite each other. `xfs_repair`, after exactly
//! that:
//!
//! ```text
//! data fork in ino 134 claims free block 24
//! ```
//!
//! # What the kernel does, measured
//!
//! An 8-block file at group block 24, then `cp --reflink=always`, then
//! truncating each copy in turn. Read back with `xfs_db`:
//!
//! ```text
//! one file          (no record — an unshared extent has none)
//! after the copy    [24,8,2,0]        startblock, blockcount, refcount, cowflag
//! first truncated   (no record)       and the free space is UNCHANGED
//! second truncated  free space gains  [24,25576]
//! ```
//!
//! Three things follow, and all three matter:
//!
//! 1. an extent with one owner has no record at all, so a missing record
//!    means unshared rather than unknown;
//! 2. freeing a shared extent decrements, and at one owner the record
//!    goes — the blocks stay put;
//! 3. only the last owner's free returns the blocks.

use crate::error::{Error, Result};
use crate::group_write::btree;

/// `sizeof(struct xfs_refcount_rec)`.
pub const RECORD: usize = 12;

/// `XFS_REFC_COWFLAG` — the record describes a copy-on-write staging
/// extent rather than a shared one. They live in the same tree, above
/// the ordinary records, and are not something this driver produces.
pub const COW_FLAG: u32 = 1 << 31;

/// One record: an extent, and how many files hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refcount {
    /// Group-relative. The top bit is the COW flag, kept out of here and
    /// in `cow` so the block number is a block number.
    pub startblock: u32,
    pub blockcount: u32,
    pub refcount: u32,
    pub cow: bool,
}

/// The records of a single-level tree, read straight out of its root.
pub fn leaf_records(buf: &[u8], numrecs: u16) -> Vec<Refcount> {
    (0..usize::from(numrecs))
        .map(|i| {
            let at = btree::V5_BODY + i * RECORD;
            let raw = u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes"));
            Refcount {
                startblock: raw & !COW_FLAG,
                blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
                refcount: u32::from_be_bytes(buf[at + 8..at + 12].try_into().expect("4 bytes")),
                cow: raw & COW_FLAG != 0,
            }
        })
        .collect()
}

/// A tree root rewritten to hold `records`, with its count brought up to
/// date.
pub fn rebuild_leaf(original: &[u8], records: &[Refcount]) -> Vec<u8> {
    let mut out = original.to_vec();
    out[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (i, r) in records.iter().enumerate() {
        let at = btree::V5_BODY + i * RECORD;
        let start = if r.cow {
            r.startblock | COW_FLAG
        } else {
            r.startblock
        };
        out[at..at + 4].copy_from_slice(&start.to_be_bytes());
        out[at + 4..at + 8].copy_from_slice(&r.blockcount.to_be_bytes());
        out[at + 8..at + 12].copy_from_slice(&r.refcount.to_be_bytes());
    }
    // The checksum is deliberately left stale; recovery recomputes it.
    out
}

/// How many records a v5 tree root of this block size can hold.
pub fn capacity(blocksize: u32) -> usize {
    (blocksize as usize - btree::V5_BODY) / RECORD
}

/// What letting go of an extent means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    /// Nobody else holds it: return the blocks to free space.
    Free,
    /// Somebody else still holds it. The record has been decremented or
    /// removed, and the blocks stay where they are.
    StillShared,
}

/// Give up one reference to `startblock..+blockcount`.
///
/// Returns whether the blocks may go back to free space. An extent with
/// no record has one owner and no record is needed to say so, which is
/// why a miss is `Free` rather than an error.
///
/// # What this refuses
///
/// A record that covers only part of the extent, or more than it.
/// Sharing a piece of a file is perfectly legal and this driver frees
/// whole extents, so the two lining up exactly is the only case it has
/// evidence for — splitting a refcount record is a shape nobody here
/// has measured.
///
/// A COW staging record over the same blocks is refused for the same
/// reason: this driver does not do copy-on-write, so one being there
/// means something else is going on.
pub fn release(records: &mut Vec<Refcount>, startblock: u32, blockcount: u32) -> Result<Release> {
    let end = u64::from(startblock) + u64::from(blockcount);

    if let Some(cow) = records
        .iter()
        .find(|r| r.cow && overlaps(r, startblock, end))
    {
        return Err(Error::UnsupportedFeature(format!(
            "group blocks {startblock}..{end} overlap a copy-on-write staging record at \
             {}..{}; this driver does not do copy-on-write and will not free underneath one",
            cow.startblock,
            u64::from(cow.startblock) + u64::from(cow.blockcount)
        )));
    }

    let Some(at) = records
        .iter()
        .position(|r| !r.cow && overlaps(r, startblock, end))
    else {
        // No record: one owner, and it is letting go.
        return Ok(Release::Free);
    };

    let rec = records[at];
    if rec.startblock != startblock || rec.blockcount != blockcount {
        return Err(Error::UnsupportedFeature(format!(
            "freeing group blocks {startblock}..{end} but the reference-count record covers \
             {}..{}; freeing part of a shared extent would have to split the record, which \
             is not implemented",
            rec.startblock,
            u64::from(rec.startblock) + u64::from(rec.blockcount)
        )));
    }

    match rec.refcount {
        0 | 1 => Err(Error::UnsupportedFeature(format!(
            "the reference-count record for group blocks {startblock}..{end} says {} owners, \
             and a record should only exist while there is more than one",
            rec.refcount
        ))),
        // Two owners becoming one: the extent stops being shared, so the
        // record goes and the blocks stay with whoever is left.
        2 => {
            records.remove(at);
            Ok(Release::StillShared)
        }
        n => {
            records[at].refcount = n - 1;
            Ok(Release::StillShared)
        }
    }
}

fn overlaps(r: &Refcount, startblock: u32, end: u64) -> bool {
    let rec_end = u64::from(r.startblock) + u64::from(r.blockcount);
    u64::from(r.startblock) < end && u64::from(startblock) < rec_end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record the kernel wrote after `cp --reflink=always` of an
    /// 8-block file at group block 24.
    fn shared() -> Vec<Refcount> {
        vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 2,
            cow: false,
        }]
    }

    #[test]
    fn a_leaf_survives_being_read_and_written_back() {
        let want = shared();
        let block = rebuild_leaf(&vec![0u8; 4096], &want);
        assert_eq!(leaf_records(&block, 1), want);
    }

    /// The COW flag rides in the top bit of the start block, so a record
    /// carrying it must not come back with an enormous block number.
    #[test]
    fn the_cow_flag_is_not_part_of_the_block_number() {
        let staging = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: true,
        }];
        let block = rebuild_leaf(&vec![0u8; 4096], &staging);
        let read = leaf_records(&block, 1);
        assert_eq!(read[0].startblock, 24, "the block number is the low bits");
        assert!(read[0].cow, "and the flag is kept, not folded in");
        assert_eq!(read, staging);
    }

    /// An extent nothing shares has no record, and letting go of it
    /// frees the blocks.
    #[test]
    fn an_unshared_extent_frees() {
        let mut records = Vec::new();
        assert_eq!(release(&mut records, 24, 8).unwrap(), Release::Free);
    }

    /// The measured sequence: two owners, then one, then none.
    #[test]
    fn the_last_owner_is_the_one_that_frees() {
        let mut records = shared();

        // The first file lets go: the record goes with it and the blocks
        // stay, because the second file still points at them.
        assert_eq!(
            release(&mut records, 24, 8).unwrap(),
            Release::StillShared,
            "two owners becoming one must not free"
        );
        assert!(records.is_empty(), "an unshared extent keeps no record");

        // The second file lets go: nothing holds them now.
        assert_eq!(
            release(&mut records, 24, 8).unwrap(),
            Release::Free,
            "the last owner frees"
        );
    }

    /// Three owners decrement rather than dropping the record.
    #[test]
    fn a_third_owner_leaves_the_record_behind() {
        let mut records = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 3,
            cow: false,
        }];
        assert_eq!(release(&mut records, 24, 8).unwrap(), Release::StillShared);
        assert_eq!(records[0].refcount, 2, "one fewer owner, and still shared");
    }

    #[test]
    fn what_is_not_implemented_is_refused_rather_than_guessed() {
        // Part of a shared extent: splitting the record is not written.
        let mut records = shared();
        let err = release(&mut records, 24, 4).unwrap_err();
        assert!(
            format!("{err}").contains("part of a shared extent"),
            "got: {err}"
        );
        assert_eq!(records, shared(), "and nothing was changed");

        // A staging extent means copy-on-write, which this does not do.
        let mut staging = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: true,
        }];
        let err = release(&mut staging, 24, 8).unwrap_err();
        assert!(format!("{err}").contains("copy-on-write"), "got: {err}");

        // A record claiming one owner should not exist.
        let mut bad = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: false,
        }];
        let err = release(&mut bad, 24, 8).unwrap_err();
        assert!(format!("{err}").contains("more than one"), "got: {err}");
    }
}
