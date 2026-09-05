//! Editing an allocation group's metadata inside a transaction.
//!
//! Freeing an extent and allocating one are the same piece of work in
//! opposite directions: read the group header and the two free-space
//! tree roots, change the records, and log which bytes of each block
//! changed. What differs between them is only which way the records
//! move, so everything else lives here rather than twice.
//!
//! # The one rule that shapes all of it
//!
//! A buffer item logs whole 128-byte chunks. Marking one byte dirty
//! puts the 127 around it into the log too, so those bytes have to be
//! *correct* rather than merely uninteresting — which is why
//! [`changed_chunks`] takes the block before and after and compares
//! them, instead of taking a caller's account of what it meant to
//! change. A field written and then written back is correctly not
//! logged; a field changed as a side effect is correctly logged.
//!
//! It also keeps records small without trying to. Removing a record
//! from a tree root changes the count and the checksum and nothing
//! else, so one chunk is logged rather than the whole block.

use crate::alloc_btree::FreeExtent;
use crate::buf_write::BufferItem;
use crate::error::{Error, Result};
use crate::format::log_items::buf_log_format::BLF_CHUNK;
use crate::inode_btree::InodeChunk;
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// Byte offsets within the allocation-group header that a free changes.
pub mod agf {
    /// `agf_freeblks`.
    pub const FREEBLKS: usize = 52;
    /// `agf_longest`.
    pub const LONGEST: usize = 56;
    /// `agf_crc`.
    pub const CRC: usize = 216;
}

/// Byte offsets within a short-form B+tree block.
pub mod btree {
    /// `bb_numrecs`.
    pub const NUMRECS: usize = 6;
    /// `bb_crc`.
    pub const CRC: usize = 52;
    /// Where the records begin in a v5 block.
    pub const V5_BODY: usize = 56;
    /// A record: a start block and a length.
    pub const RECORD: usize = 8;
}

/// Offsets within the on-disk inode core.
pub mod inode {
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    /// `di_nextents`, when the extent counts are 32-bit.
    pub const NEXTENTS: usize = 76;
    /// The 64-bit data-extent count, under the `nrext64` feature.
    pub const NEXTENTS64: usize = 24;
    pub const CHANGECOUNT: usize = 104;
    pub const FLAGS2: usize = 120;
}

/// Recompute a metadata block's checksum in place.
///
/// The checksum covers the block with its own checksum field zeroed, so
/// it cannot be computed until everything else is final.
///
/// # Why almost nothing here calls this
///
/// **A logged block does not carry a correct checksum, and must not.**
/// Recovery recomputes it when it writes the block out, which is after
/// the log has been applied — so the checksum in a record is stale by
/// construction, and the kernel's own records carry stale ones.
///
/// Stamping a correct one anyway is not wrong, but it makes the record
/// *bigger*: the checksum of an allocation-group header sits in a
/// 128-byte chunk nothing else in the transaction touches, so writing it
/// turns one dirty run into two and adds an operation to the record. The
/// kernel's own group-header items are one run in 619 of the 620 in the
/// corpus, which is what says it does not stamp them either.
///
/// Left here because a block written outside a transaction — where
/// nothing will recompute it — does need one.
pub fn restamp_crc(buf: &mut [u8], crc_off: usize) {
    buf[crc_off..crc_off + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c_with_zeroed_crc(buf, crc_off);
    buf[crc_off..crc_off + 4].copy_from_slice(&crc.to_le_bytes());
}

/// A buffer item covering `after`, with every 128-byte chunk that
/// differs from `before` marked dirty.
///
/// Comparing the two is the definition of what the item is for — which
/// bytes of this buffer changed — rather than a caller's account of what
/// it meant to change. A field written and then written back to its old
/// value is correctly not logged; a field changed as a side effect is
/// correctly logged.
pub fn changed_chunks(blkno: u64, before: &[u8], after: Vec<u8>, buf_type: u16) -> BufferItem {
    debug_assert_eq!(before.len(), after.len());
    let mut item = BufferItem::new(blkno, after, buf_type, 0);
    for chunk in 0..before.len().div_ceil(BLF_CHUNK) {
        let from = chunk * BLF_CHUNK;
        let to = (from + BLF_CHUNK).min(before.len());
        if before[from..to] != item.data()[from..to] {
            item.mark(from, to - from);
        }
    }
    item
}

/// The records of a single-level free-space tree, read straight out of
/// its root.
pub fn leaf_records(buf: &[u8], numrecs: u16) -> Vec<FreeExtent> {
    (0..usize::from(numrecs))
        .map(|i| {
            let at = btree::V5_BODY + i * btree::RECORD;
            FreeExtent {
                startblock: u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes")),
                blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
            }
        })
        .collect()
}

/// A tree root rewritten to hold `records`, with its count brought up to
/// date.
///
/// Records past the new count are left as they are rather than cleared.
/// They are unreachable — `bb_numrecs` says where the records stop — and
/// leaving them alone keeps the change to the bytes that actually
/// changed, which is the difference between logging one chunk and
/// logging the whole block.
pub fn rebuild_leaf(original: &[u8], records: &[FreeExtent]) -> Vec<u8> {
    let mut out = original.to_vec();
    out[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (i, record) in records.iter().enumerate() {
        let at = btree::V5_BODY + i * btree::RECORD;
        out[at..at + 4].copy_from_slice(&record.startblock.to_be_bytes());
        out[at + 4..at + 8].copy_from_slice(&record.blockcount.to_be_bytes());
    }
    // The checksum is deliberately left stale; see `restamp_crc`.
    out
}

/// A record of an inode B+tree, in every version.
pub const INODE_RECORD_LEN: usize = 16;

/// A tree root rewritten to hold `chunks`.
///
/// The record shape follows the sparse-inodes feature, not the format
/// version — see [`crate::inode_btree`].
pub fn rebuild_inode_leaf(original: &[u8], chunks: &[InodeChunk], sparse: bool) -> Vec<u8> {
    let mut out = original.to_vec();
    out[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&(chunks.len() as u16).to_be_bytes());
    for (i, c) in chunks.iter().enumerate() {
        let at = btree::V5_BODY + i * INODE_RECORD_LEN;
        out[at..at + 4].copy_from_slice(&c.startino.to_be_bytes());
        if sparse {
            out[at + 4..at + 6].copy_from_slice(&c.holemask.to_be_bytes());
            out[at + 6] = c.count;
            out[at + 7] = c.freecount;
        } else {
            out[at + 4..at + 8].copy_from_slice(&u32::from(c.freecount).to_be_bytes());
        }
        out[at + 8..at + 16].copy_from_slice(&c.free.to_be_bytes());
    }
    // The checksum is deliberately left stale; recovery recomputes it.
    // See `group_write::restamp_crc`.
    out
}

/// How many records a v5 tree root of this block size can hold.
pub fn leaf_capacity(blocksize: u32) -> usize {
    (blocksize as usize - btree::V5_BODY) / btree::RECORD
}

/// The inode core a truncated file has: no size, no blocks, no extents.
pub fn emptied_core(raw: &[u8], v5: bool) -> Vec<u8> {
    let mut core = raw.to_vec();
    core[inode::SIZE..inode::SIZE + 8].copy_from_slice(&0u64.to_be_bytes());
    core[inode::NBLOCKS..inode::NBLOCKS + 8].copy_from_slice(&0u64.to_be_bytes());

    // Where the data-extent count lives depends on a feature bit in the
    // inode itself rather than in the superblock, because it is the
    // inode's own encoding that matters.
    let nrext64 = v5
        && u64::from_be_bytes(
            raw[inode::FLAGS2..inode::FLAGS2 + 8]
                .try_into()
                .expect("8 bytes"),
        ) & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
            != 0;
    if nrext64 {
        core[inode::NEXTENTS64..inode::NEXTENTS64 + 8].copy_from_slice(&0u64.to_be_bytes());
    } else {
        core[inode::NEXTENTS..inode::NEXTENTS + 4].copy_from_slice(&0u32.to_be_bytes());
    }

    if v5 {
        let at = inode::CHANGECOUNT;
        let now = u64::from_be_bytes(core[at..at + 8].try_into().expect("8 bytes"));
        core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
    }
    core
}

/// Which allocation group a filesystem block is in, and where inside it.
pub fn split_fsblock(sb: &Superblock, fsblock: u64) -> (u32, u32) {
    (
        (fsblock >> sb.agblklog) as u32,
        (fsblock & ((1u64 << sb.agblklog) - 1)) as u32,
    )
}

// ---------------------------------------------------------------------
// Taking blocks out of a group
// ---------------------------------------------------------------------

/// Blocks taken out of an allocation group, and the items that say so.
pub struct Allocated {
    /// Where the run starts, relative to the group.
    pub agblock: u32,
    /// The group header, the by-block tree, the by-length tree and —
    /// where the filesystem has one — the reverse-mapping tree, in that
    /// order.
    pub items: Vec<BufferItem>,
}

impl crate::fs::Filesystem {
    /// Take `want` contiguous blocks out of allocation group `agno`, for
    /// `owner` at file offset `offset`.
    ///
    /// Returns where they start and the buffer items recording the
    /// change. Nothing is written: the items are the change, and the
    /// caller puts them in a record.
    ///
    /// # Why the owner is an argument
    ///
    /// Where the filesystem has a reverse-mapping tree, an allocation is
    /// not complete until the tree says who the blocks belong to. That
    /// is not something an allocator can work out — only the caller
    /// knows what it is allocating for — so it comes in with the
    /// request rather than being inferred here.
    ///
    /// # Which blocks
    ///
    /// The first free run long enough, in block order. That policy is
    /// this driver's rather than XFS's — XFS weighs locality,
    /// contiguity and several other things, none of which is visible in
    /// a record. Any run that is genuinely free produces a filesystem
    /// the kernel accepts, so the choice affects layout rather than
    /// correctness.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when the group's free-space trees
    /// are more than one level deep, when no single run is long enough,
    /// or when the result would need more records than a tree root
    /// holds.
    pub(crate) fn allocate_in_group(
        &self,
        agno: u32,
        want: u32,
        owner: i64,
        offset: u64,
    ) -> Result<Allocated> {
        use crate::ag::agf_btree::{BNO, CNT, RMAP};
        use crate::ag::Agf;
        use crate::alloc_btree::{alloc_extent, expected_blkno, longest, total_free, FreeExtent};
        use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGF, BLFT_BTREE};
        use crate::log::BBSIZE;

        let blocksize = u64::from(self.sb.blocksize);
        let sector = u64::from(self.sb.sectsize);
        let ag_start = u64::from(agno) * u64::from(self.sb.agblocks) * blocksize;

        let mut agf_raw = vec![0u8; self.sb.sectsize as usize];
        self.device().read_at(ag_start + sector, &mut agf_raw)?;
        let agf = Agf::parse(&agf_raw, &self.sb, agno)?;

        for (which, name) in [(BNO, "by-block"), (CNT, "by-length")] {
            if agf.levels[which] != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno}'s {name} free-space tree is {} levels deep, \
                     where taking a record out can collapse a node; only a single-level \
                     tree is supported",
                    agf.levels[which]
                )));
            }
        }

        let mut bno_raw = vec![0u8; self.sb.blocksize as usize];
        self.device().read_at(
            ag_start + u64::from(agf.roots[BNO]) * blocksize,
            &mut bno_raw,
        )?;
        let mut cnt_raw = vec![0u8; self.sb.blocksize as usize];
        self.device().read_at(
            ag_start + u64::from(agf.roots[CNT]) * blocksize,
            &mut cnt_raw,
        )?;

        let numrecs = u16::from_be_bytes(
            bno_raw[btree::NUMRECS..btree::NUMRECS + 2]
                .try_into()
                .expect("2 bytes"),
        );
        let mut by_block = leaf_records(&bno_raw, numrecs);

        let chosen = by_block
            .iter()
            .find(|run| run.blockcount >= want)
            .copied()
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "allocation group {agno} has no single free run of {want} blocks — its \
                     longest is {}, and splitting across extents is not implemented",
                    longest(&by_block)
                ))
            })?;
        let taking = FreeExtent {
            startblock: chosen.startblock,
            blockcount: want,
        };
        alloc_extent(&mut by_block, taking)?;

        let capacity = leaf_capacity(self.sb.blocksize);
        if by_block.len() > capacity {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno} would need {} free-space records and its tree root \
                 holds {capacity}; splitting a node is not implemented",
                by_block.len()
            )));
        }

        let mut by_count = by_block.clone();
        by_count.sort_by_key(|e| (e.blockcount, e.startblock));

        let new_bno = rebuild_leaf(&bno_raw, &by_block);
        let new_cnt = rebuild_leaf(&cnt_raw, &by_count);

        let mut new_agf = agf_raw.clone();
        let freeblks = u32::try_from(total_free(&by_block)).map_err(|_| {
            Error::CorruptLog(format!(
                "allocation group {agno} has more free blocks than fit"
            ))
        })?;
        new_agf[agf::FREEBLKS..agf::FREEBLKS + 4].copy_from_slice(&freeblks.to_be_bytes());
        new_agf[agf::LONGEST..agf::LONGEST + 4].copy_from_slice(&longest(&by_block).to_be_bytes());
        // The checksum is left stale on purpose — recovery recomputes it.

        let ag_bb = ag_start / BBSIZE as u64;
        // The reverse map, where the filesystem has one. Blocks that
        // have just left free space belong to `owner` from here on, and
        // a tree that does not say so describes a filesystem where they
        // belong to nobody.
        let mut items = vec![
            changed_chunks(ag_bb + sector / BBSIZE as u64, &agf_raw, new_agf, BLFT_AGF),
            changed_chunks(
                expected_blkno(&self.sb, agno, agf.roots[BNO]),
                &bno_raw,
                new_bno,
                BLFT_BTREE,
            ),
            changed_chunks(
                expected_blkno(&self.sb, agno, agf.roots[CNT]),
                &cnt_raw,
                new_cnt,
                BLFT_BTREE,
            ),
        ];

        if self.sb.has_rmapbt() {
            if agf.levels[RMAP] != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno}'s reverse-mapping tree is {} levels deep, \
                     where inserting a record can split a node; only a single-level tree \
                     is supported",
                    agf.levels[RMAP]
                )));
            }
            let mut rmap_raw = vec![0u8; self.sb.blocksize as usize];
            self.device().read_at(
                ag_start + u64::from(agf.roots[RMAP]) * blocksize,
                &mut rmap_raw,
            )?;
            let n = u16::from_be_bytes(
                rmap_raw[btree::NUMRECS..btree::NUMRECS + 2]
                    .try_into()
                    .expect("2 bytes"),
            );
            let mut records = crate::rmap::leaf_records(&rmap_raw, n);
            crate::rmap::insert(
                &mut records,
                crate::rmap::Rmap {
                    startblock: taking.startblock,
                    blockcount: taking.blockcount,
                    owner,
                    offset,
                },
            )?;
            let rmap_capacity = crate::rmap::capacity(self.sb.blocksize);
            if records.len() > rmap_capacity {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno} would need {} reverse-mapping records and its \
                     tree root holds {rmap_capacity}; splitting a node is not implemented",
                    records.len()
                )));
            }
            let new_rmap = crate::rmap::rebuild_leaf(&rmap_raw, &records);
            items.push(changed_chunks(
                expected_blkno(&self.sb, agno, agf.roots[RMAP]),
                &rmap_raw,
                new_rmap,
                BLFT_BTREE,
            ));
        }

        Ok(Allocated {
            agblock: taking.startblock,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::log_items::buf_log_format::buf_type::BLFT_BTREE;

    /// Only the chunks that differ are logged, and a byte written back
    /// to what it already was is not a change.
    #[test]
    fn only_the_changed_chunks_are_logged() {
        let before = vec![7u8; 4096];

        let mut after = before.clone();
        after[BLF_CHUNK * 3 + 10] = 9;
        let item = changed_chunks(8, &before, after, BLFT_BTREE);
        let ops = item.ops();
        assert_eq!(ops.len(), 2, "one run of one chunk");
        assert_eq!(ops[1].data.len(), BLF_CHUNK);

        // Written and written back: nothing changed, so nothing is
        // logged and the item is the format operation alone.
        let unchanged = changed_chunks(8, &before, before.clone(), BLFT_BTREE);
        assert_eq!(unchanged.ops().len(), 1);
    }

    /// A rebuilt root carries the new count and the new records — and
    /// deliberately *not* a checksum that covers them, because a logged
    /// block's checksum is recomputed by recovery and stamping one here
    /// would only make the record bigger.
    #[test]
    fn a_rebuilt_root_carries_the_records_and_a_stale_checksum() {
        let mut original = vec![0u8; 4096];
        original[0..4].copy_from_slice(&0x4142_3342u32.to_be_bytes());
        restamp_crc(&mut original, btree::CRC);

        let records = [
            FreeExtent {
                startblock: 10,
                blockcount: 6,
            },
            FreeExtent {
                startblock: 536,
                blockcount: 256,
            },
        ];
        let rebuilt = rebuild_leaf(&original, &records);

        let numrecs = u16::from_be_bytes(
            rebuilt[btree::NUMRECS..btree::NUMRECS + 2]
                .try_into()
                .unwrap(),
        );
        assert_eq!(numrecs, 2);
        assert_eq!(leaf_records(&rebuilt, numrecs), records);

        // The original's checksum survives untouched: it no longer
        // matches the block, and that is the point.
        assert_eq!(
            rebuilt[btree::CRC..btree::CRC + 4],
            original[btree::CRC..btree::CRC + 4],
            "the checksum should be carried across, not recomputed"
        );
        assert_ne!(
            u32::from_le_bytes(rebuilt[btree::CRC..btree::CRC + 4].try_into().unwrap()),
            crc32c_with_zeroed_crc(&rebuilt, btree::CRC),
            "and it should no longer cover the block, which is what recovery fixes"
        );
    }

    /// The records past the new count are left alone, so removing one
    /// changes only the count rather than the whole block.
    #[test]
    fn shrinking_a_root_leaves_the_tail_alone() {
        let mut original = vec![0u8; 4096];
        let three = [
            FreeExtent {
                startblock: 1,
                blockcount: 1,
            },
            FreeExtent {
                startblock: 3,
                blockcount: 1,
            },
            FreeExtent {
                startblock: 5,
                blockcount: 1,
            },
        ];
        original = rebuild_leaf(&original, &three);

        let two = rebuild_leaf(&original, &three[..2]);
        let item = changed_chunks(8, &original, two, BLFT_BTREE);
        // The count and the checksum are both in the first chunk, and
        // nothing else moved.
        assert_eq!(item.ops().len(), 2);
        assert_eq!(item.ops()[1].data.len(), BLF_CHUNK);
    }

    /// A v5 root of the usual size holds 505 records.
    #[test]
    fn a_root_holds_as_many_records_as_fit() {
        assert_eq!(leaf_capacity(4096), 505);
        assert_eq!(leaf_capacity(1024), 121);
    }
}
