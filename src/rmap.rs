//! The reverse-mapping B+tree: which extent belongs to whom.
//!
//! `rmapbt` records, for every extent in an allocation group, what owns
//! it. The kernel keeps one record per contiguous run, so the tree
//! answers "who has block N" without walking every inode — which is what
//! `xfs_repair` and the online scrubber use to check everything else.
//!
//! # Why a driver that writes has to care
//!
//! Every allocation adds a record and every free removes one. A driver
//! that allocates without adding leaves a block owned by nobody; one
//! that frees without removing leaves a record pointing at free space.
//! Neither fails at the time. `xfs_repair` is what notices:
//!
//! ```text
//! Missing reverse-mapping record for (0/13) len 1 owner 131 off 0
//! ```
//!
//! This driver refused a read-write mount of such a filesystem rather
//! than write one wrong. `mkfs.xfs` turns the feature on by default,
//! though, so refusing meant refusing an ordinary volume.
//!
//! # The record, as the kernel writes it
//!
//! Measured on a filesystem the kernel populated, read back with
//! `xfs_db`. A one-block file at group block 12 owned by inode 131:
//!
//! ```text
//! recs[5] = [startblock,blockcount,owner,offset,extentflag,attrfork,bmbtblock]
//!           [12,1,131,0,0,0,0]
//! ```
//!
//! Owners below zero are the reserved ones — `-3` the filesystem's own
//! headers, `-5` the free-space trees, `-6` the inode tree, `-7` the
//! inode chunks — so the field is signed.
//!
//! The three flags live in the top bits of `rm_offset` rather than in a
//! field of their own, which is why this keeps the offset raw.

use crate::error::{Error, Result};
use crate::group_write::btree;

/// `sizeof(struct xfs_rmap_rec)`.
pub const RECORD: usize = 24;

/// The reserved owners, which are negative so they cannot be mistaken
/// for an inode number.
///
/// Read off a filesystem the kernel populated: `-3` beside the group's
/// own headers, `-5` beside the free-space trees, `-6` beside the inode
/// tree, `-7` beside the inode chunks.
pub const OWN_FS: i64 = -3;
/// `XFS_RMAP_OWN_AG` — the free-space trees and the free list.
pub const OWN_AG: i64 = -5;
/// `XFS_RMAP_OWN_INOBT` — the inode B+trees.
pub const OWN_INOBT: i64 = -6;
/// `XFS_RMAP_OWN_INODES` — the blocks that hold inode chunks.
pub const OWN_INODES: i64 = -7;

/// `XFS_RMAP_OFF_ATTR_FORK` — the extent belongs to the attribute fork.
pub const OFF_ATTR_FORK: u64 = 1 << 63;
/// `XFS_RMAP_OFF_BMBT_BLOCK` — a block of the inode's own map, not data.
pub const OFF_BMBT_BLOCK: u64 = 1 << 62;
/// `XFS_RMAP_OFF_UNWRITTEN` — allocated but never written.
pub const OFF_UNWRITTEN: u64 = 1 << 61;
/// What is left of `rm_offset` once the flags are taken out.
pub const OFF_MASK: u64 = (1 << 54) - 1;

/// One record: an extent, and what owns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rmap {
    /// Group-relative, like every other record in an AG tree.
    pub startblock: u32,
    pub blockcount: u32,
    /// An inode number, or one of the reserved negative owners.
    pub owner: i64,
    /// `rm_offset` whole, flags included. Kept raw because the flags
    /// have no field of their own and splitting them here would mean
    /// putting them back together on the way out.
    pub offset: u64,
}

impl Rmap {
    /// The file offset alone.
    pub fn file_offset(&self) -> u64 {
        self.offset & OFF_MASK
    }

    /// The flags alone.
    pub fn flags(&self) -> u64 {
        self.offset & !OFF_MASK
    }

    /// Whether this record is one of the reserved owners rather than an
    /// inode.
    ///
    /// It decides what "continues" means. A file's extents carry the
    /// offset within the file, so two records only join when the second
    /// starts where the first left off. The reserved owners have no such
    /// thing — the filesystem's headers are not at an offset within
    /// anything — and their offset is always zero, so adjacency on disk
    /// is the whole test.
    ///
    /// Getting that wrong is how a merge went undetected: two inode
    /// chunks at blocks 16 and 24, both owned by -7, both at offset
    /// zero. `0 + 8 == 0` is false, so they looked unrelated, and
    /// xfs_repair said otherwise -- `record 8 in block (0/5) of rmap
    /// tree should be merged with previous record`.
    fn is_reserved_owner(&self) -> bool {
        self.owner < 0
    }

    /// Whether `other` continues this record exactly: the same owner and
    /// flags, starting at the block after this one ends, and — for a
    /// file — at the file offset after this one ends.
    fn continues_into(&self, other: &Rmap) -> bool {
        if self.owner != other.owner || self.flags() != other.flags() {
            return false;
        }
        if self.startblock.checked_add(self.blockcount) != Some(other.startblock) {
            return false;
        }
        self.is_reserved_owner()
            || self.file_offset() + u64::from(self.blockcount) == other.file_offset()
    }
}

/// The records of a single-level tree, read straight out of its root.
pub fn leaf_records(buf: &[u8], numrecs: u16) -> Vec<Rmap> {
    (0..usize::from(numrecs))
        .map(|i| {
            let at = btree::V5_BODY + i * RECORD;
            Rmap {
                startblock: u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes")),
                blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
                owner: i64::from_be_bytes(buf[at + 8..at + 16].try_into().expect("8 bytes")),
                offset: u64::from_be_bytes(buf[at + 16..at + 24].try_into().expect("8 bytes")),
            }
        })
        .collect()
}

/// A tree root rewritten to hold `records`, with its count brought up to
/// date.
///
/// Records past the new count are left as they are rather than cleared,
/// for the reason `group_write::rebuild_leaf` gives: they are
/// unreachable, and leaving them alone keeps the change to the bytes
/// that actually changed.
pub fn rebuild_leaf(original: &[u8], records: &[Rmap]) -> Vec<u8> {
    let mut out = original.to_vec();
    out[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (i, r) in records.iter().enumerate() {
        let at = btree::V5_BODY + i * RECORD;
        out[at..at + 4].copy_from_slice(&r.startblock.to_be_bytes());
        out[at + 4..at + 8].copy_from_slice(&r.blockcount.to_be_bytes());
        out[at + 8..at + 16].copy_from_slice(&r.owner.to_be_bytes());
        out[at + 16..at + 24].copy_from_slice(&r.offset.to_be_bytes());
    }
    // The checksum is deliberately left stale; recovery recomputes it.
    // See `group_write::restamp_crc`.
    out
}

/// How many records a v5 tree root of this block size can hold.
pub fn capacity(blocksize: u32) -> usize {
    (blocksize as usize - btree::V5_BODY) / RECORD
}

/// Add the record for a newly allocated extent.
///
/// Records are ordered by start block, and the tree holds no two records
/// for the same one, so the position is decided by the extent itself.
///
/// # Merging
///
/// The kernel keeps ONE record per contiguous run, so an extent that
/// continues its neighbour extends that record rather than adding
/// another beside it. Measured: growing a file from one block to four
/// turned `[12,1,131,0]` into `[12,4,131,0]`.
///
/// This refused that case until an inode chunk produced it. A chunk's
/// blocks land next to the chunk before them and carry the same reserved
/// owner, so allocating one on a filesystem with a reverse map is a
/// merge every time — and `xfs_repair` says so:
///
/// ```text
/// record 8 in block (0/5) of rmap tree should be merged with previous record
/// ```
///
/// Both neighbours are considered, and both can join at once: an extent
/// that exactly fills a gap between two records of the same owner
/// collapses all three into one.
pub fn insert(records: &mut Vec<Rmap>, rec: Rmap) -> Result<()> {
    let at = records
        .iter()
        .position(|r| r.startblock > rec.startblock)
        .unwrap_or(records.len());

    if let Some(before) = at.checked_sub(1).and_then(|i| records.get(i)) {
        let end = u64::from(before.startblock) + u64::from(before.blockcount);
        if end > u64::from(rec.startblock) {
            return Err(Error::UnsupportedFeature(format!(
                "an extent at group block {} overlaps the reverse-mapping record for \
                 {}..{} owned by {}",
                rec.startblock, before.startblock, end, before.owner
            )));
        }
    }

    let joins_before = at
        .checked_sub(1)
        .and_then(|i| records.get(i))
        .is_some_and(|before| before.continues_into(&rec));
    let joins_after = records
        .get(at)
        .is_some_and(|after| rec.continues_into(after));

    match (joins_before, joins_after) {
        // Fills the gap exactly: three records become one.
        (true, true) => {
            let after = records.remove(at);
            let before = &mut records[at - 1];
            before.blockcount = before
                .blockcount
                .checked_add(rec.blockcount)
                .and_then(|n| n.checked_add(after.blockcount))
                .ok_or_else(|| {
                    Error::UnsupportedFeature(
                        "merging three reverse-mapping records would overflow the block count"
                            .into(),
                    )
                })?;
        }
        // Extends the record before it.
        (true, false) => {
            let before = &mut records[at - 1];
            before.blockcount = before
                .blockcount
                .checked_add(rec.blockcount)
                .ok_or_else(|| {
                    Error::UnsupportedFeature(
                        "merging reverse-mapping records would overflow the block count".into(),
                    )
                })?;
        }
        // Extends the record after it, which now starts earlier.
        (false, true) => {
            let after = &mut records[at];
            after.blockcount = after
                .blockcount
                .checked_add(rec.blockcount)
                .ok_or_else(|| {
                    Error::UnsupportedFeature(
                        "merging reverse-mapping records would overflow the block count".into(),
                    )
                })?;
            after.startblock = rec.startblock;
            after.offset = rec.offset;
        }
        (false, false) => records.insert(at, rec),
    }
    Ok(())
}

/// Remove the record for a freed extent.
///
/// # What this refuses
///
/// Anything but an exact match. Freeing part of an extent leaves the
/// rest, which means shortening a record or splitting it in two, and
/// this driver frees whole extents only — `truncate_to_zero` frees a
/// file's map entire. A partial free arriving here means something
/// upstream changed, and saying so is better than trimming a record on a
/// guess.
pub fn remove(records: &mut Vec<Rmap>, rec: Rmap) -> Result<()> {
    let at = records
        .iter()
        .position(|r| {
            r.startblock == rec.startblock && r.owner == rec.owner && r.flags() == rec.flags()
        })
        .ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "freeing group block {} owned by {} but the reverse-mapping tree has no \
                 record starting there; the tree and the inode disagree about what this \
                 extent is",
                rec.startblock, rec.owner
            ))
        })?;

    if records[at].blockcount != rec.blockcount {
        return Err(Error::UnsupportedFeature(format!(
            "freeing {} blocks at group block {} but its reverse-mapping record covers {}; \
             freeing part of an extent is not implemented",
            rec.blockcount, rec.startblock, records[at].blockcount
        )));
    }
    records.remove(at);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel's own leaf, byte for byte, as `xfs_db` printed it for
    /// a freshly populated group. Six records: the filesystem's headers,
    /// the free-space trees twice, the inode tree, one file, and the
    /// inode chunks.
    fn kernel_leaf() -> Vec<Rmap> {
        vec![
            Rmap {
                startblock: 0,
                blockcount: 1,
                owner: -3,
                offset: 0,
            },
            Rmap {
                startblock: 1,
                blockcount: 2,
                owner: -5,
                offset: 0,
            },
            Rmap {
                startblock: 3,
                blockcount: 2,
                owner: -6,
                offset: 0,
            },
            Rmap {
                startblock: 5,
                blockcount: 7,
                owner: -5,
                offset: 0,
            },
            Rmap {
                startblock: 12,
                blockcount: 1,
                owner: 131,
                offset: 0,
            },
            Rmap {
                startblock: 16,
                blockcount: 8,
                owner: -7,
                offset: 0,
            },
        ]
    }

    fn v5_block(records: &[Rmap]) -> Vec<u8> {
        rebuild_leaf(&vec![0u8; 4096], records)
    }

    #[test]
    fn a_leaf_survives_being_read_and_written_back() {
        let want = kernel_leaf();
        let block = v5_block(&want);
        assert_eq!(leaf_records(&block, want.len() as u16), want);
    }

    /// The owner is signed. Read as unsigned, the reserved owners come
    /// back as enormous positive numbers and every comparison against
    /// them silently fails.
    #[test]
    fn the_reserved_owners_stay_negative() {
        let block = v5_block(&kernel_leaf());
        let read = leaf_records(&block, 6);
        assert_eq!(read[0].owner, -3, "the filesystem's own headers");
        assert_eq!(read[5].owner, -7, "the inode chunks");
    }

    #[test]
    fn a_new_extent_goes_in_start_block_order() {
        let mut records = kernel_leaf();
        insert(
            &mut records,
            Rmap {
                startblock: 13,
                blockcount: 2,
                owner: 140,
                offset: 0,
            },
        )
        .expect("insert");
        assert_eq!(records[5].startblock, 13);
        assert_eq!(records[6].startblock, 16, "and the rest keeps its order");
    }

    #[test]
    fn freeing_an_extent_takes_its_record_out() {
        let mut records = kernel_leaf();
        remove(
            &mut records,
            Rmap {
                startblock: 12,
                blockcount: 1,
                owner: 131,
                offset: 0,
            },
        )
        .expect("remove");
        assert_eq!(records.len(), 5);
        assert!(!records.iter().any(|r| r.owner == 131));
    }

    /// Refusals, each for a shape this driver does not produce and must
    /// not guess at.
    #[test]
    fn what_is_not_implemented_is_refused_rather_than_guessed() {
        // A partial free would have to shorten or split the record.
        let mut records = kernel_leaf();
        let err = remove(
            &mut records,
            Rmap {
                startblock: 16,
                blockcount: 4,
                owner: -7,
                offset: 0,
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("part of an extent"));
        assert_eq!(records.len(), 6, "and nothing was removed");

        // A free with no matching record means the tree and the inode
        // disagree, which is not something to paper over.
        let err = remove(
            &mut records,
            Rmap {
                startblock: 99,
                blockcount: 1,
                owner: 131,
                offset: 0,
            },
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("no \n                 record")
                || format!("{err}").contains("record starting there"),
            "got: {err}"
        );

        // An allocation on top of an extent that is already owned.
        let err = insert(
            &mut records,
            Rmap {
                startblock: 17,
                blockcount: 1,
                owner: 200,
                offset: 0,
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("overlaps"), "got: {err}");
    }

    /// An allocation that continues its neighbour extends that record
    /// rather than adding another.
    ///
    /// Measured: the kernel grew a one-block file to four and the record
    /// went from `[12,1,131,0]` to `[12,4,131,0]`.
    #[test]
    fn an_extent_that_continues_its_neighbour_extends_it() {
        let mut records = kernel_leaf();
        insert(
            &mut records,
            Rmap {
                startblock: 13,
                blockcount: 3,
                owner: 131,
                offset: 1,
            },
        )
        .expect("a continuation merges");

        assert_eq!(records.len(), 6, "no record was added");
        let file = records.iter().find(|r| r.owner == 131).expect("the file");
        assert_eq!(
            (file.startblock, file.blockcount),
            (12, 4),
            "one record covering the whole run, as the kernel wrote it"
        );
    }

    /// A reserved owner has no file offset, so being next door is the
    /// whole test.
    ///
    /// This is what an inode chunk does: its blocks land beside the
    /// chunk before them, both owned by -7 and both at offset zero.
    /// Requiring the offsets to continue made `0 + 8 == 0` the question,
    /// which is false, so the merge went unnoticed until xfs_repair said
    /// `record 8 ... should be merged with previous record`.
    #[test]
    fn a_reserved_owner_merges_on_adjacency_alone() {
        let mut records = kernel_leaf();
        // The inode chunks run 16..24; a new chunk starts exactly there.
        insert(
            &mut records,
            Rmap {
                startblock: 24,
                blockcount: 8,
                owner: -7,
                offset: 0,
            },
        )
        .expect("a chunk beside a chunk merges");

        let chunks: Vec<_> = records.iter().filter(|r| r.owner == -7).collect();
        assert_eq!(chunks.len(), 1, "one record for the inode chunks, not two");
        assert_eq!((chunks[0].startblock, chunks[0].blockcount), (16, 16));
    }

    /// An extent that exactly fills the gap between two records of the
    /// same owner collapses all three.
    #[test]
    fn filling_a_gap_between_two_records_collapses_all_three() {
        let mut records = vec![
            Rmap {
                startblock: 0,
                blockcount: 4,
                owner: 200,
                offset: 0,
            },
            Rmap {
                startblock: 10,
                blockcount: 4,
                owner: 200,
                offset: 10,
            },
        ];
        insert(
            &mut records,
            Rmap {
                startblock: 4,
                blockcount: 6,
                owner: 200,
                offset: 4,
            },
        )
        .expect("the gap is filled");
        assert_eq!(records.len(), 1, "three runs became one");
        assert_eq!((records[0].startblock, records[0].blockcount), (0, 14));
    }

    #[test]
    fn a_four_kilobyte_root_holds_a_hundred_and_sixty_eight_records() {
        assert_eq!(capacity(4096), (4096 - 56) / 24);
        assert_eq!(capacity(4096), 168);
    }
}
