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

    /// Whether `other` continues this record exactly: the same owner and
    /// flags, starting at the block after this one ends, at the file
    /// offset after this one ends.
    fn continues_into(&self, other: &Rmap) -> bool {
        self.owner == other.owner
            && self.flags() == other.flags()
            && self.startblock.checked_add(self.blockcount) == Some(other.startblock)
            && self.file_offset() + u64::from(self.blockcount) == other.file_offset()
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
/// # What this refuses
///
/// A record that would MERGE with a neighbour. The kernel keeps one
/// record per contiguous run and will merge an allocation into an
/// adjacent record of the same owner — measured, growing a file from one
/// block to four turned `[12,1,131,0]` into `[12,4,131,0]` rather than
/// adding a second record.
///
/// No operation in this driver can produce that case: blocks are only
/// allocated for a file that had none, or for a directory leaving its
/// inode, and neither has an existing extent to abut. So rather than
/// implement a merge against no evidence, this refuses one — if the case
/// ever arises it will say so instead of writing a shape that was never
/// measured.
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
        if before.continues_into(&rec) {
            return Err(Error::UnsupportedFeature(format!(
                "the extent at group block {} continues the reverse-mapping record before \
                 it, which the kernel would merge into one; merging is not implemented",
                rec.startblock
            )));
        }
    }
    if let Some(after) = records.get(at) {
        if rec.continues_into(after) {
            return Err(Error::UnsupportedFeature(format!(
                "the extent at group block {} runs into the reverse-mapping record after \
                 it, which the kernel would merge into one; merging is not implemented",
                rec.startblock
            )));
        }
    }

    records.insert(at, rec);
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

        // An allocation the kernel would merge into its neighbour.
        let err = insert(
            &mut records,
            Rmap {
                startblock: 13,
                blockcount: 1,
                owner: 131,
                offset: 1,
            },
        )
        .unwrap_err();
        assert!(format!("{err}").contains("merge"), "got: {err}");

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

    #[test]
    fn a_four_kilobyte_root_holds_a_hundred_and_sixty_eight_records() {
        assert_eq!(capacity(4096), (4096 - 56) / 24);
        assert_eq!(capacity(4096), 168);
    }
}
