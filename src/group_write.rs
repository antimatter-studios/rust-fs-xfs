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
    // Same class as the two encoders: this diff becomes the regions of a
    // buffer log item, so a length mismatch writes a journal record
    // describing bytes that are not there. Not named in the issue, but
    // it is the same `debug_assert` in the same path and leaving it
    // behind would be leaving a small version of the defect.
    assert_eq!(
        before.len(),
        after.len(),
        "a buffer diff compares {} bytes against {}",
        before.len(),
        after.len()
    );
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

/// The record count in a tree root, checked against the block holding it.
///
/// `bb_numrecs` is two bytes off a block this driver has not verified in
/// any other way -- no magic, no CRC, no owner, no self-address, unlike
/// `alloc_btree::parse_block`, which checks all four. It was then used
/// directly as a loop bound over `buf[at..at + RECORD]`, so 0xFFFF at a
/// 4 KiB block size indexed past the end and panicked: out of
/// `free_extents`, `rmap_records` and `refcount_records` alike, and out
/// of the read paths `create` and `unlink` take.
///
/// A root holds as many records as fit in it. One claiming more is not
/// describing this block.
pub fn leaf_numrecs(buf: &[u8], record_bytes: usize) -> crate::error::Result<u16> {
    let numrecs = u16::from_be_bytes(
        buf[btree::NUMRECS..btree::NUMRECS + 2]
            .try_into()
            .expect("2 bytes"),
    );
    let capacity = buf.len().saturating_sub(btree::V5_BODY) / record_bytes;
    if usize::from(numrecs) > capacity {
        return Err(crate::error::Error::CorruptLog(format!(
            "a tree root says it holds {numrecs} records, where {capacity} fit in \
             its {}-byte block",
            buf.len()
        )));
    }
    Ok(numrecs)
}

/// The records of a single-level free-space tree, read straight out of
/// its root.
///
/// `numrecs` must have come from [`leaf_numrecs`]; the `min` is a
/// backstop so a future caller that forgets cannot index past the end.
pub fn leaf_records(buf: &[u8], numrecs: u16) -> Vec<FreeExtent> {
    let fit = buf.len().saturating_sub(btree::V5_BODY) / btree::RECORD;
    (0..usize::from(numrecs).min(fit))
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

/// One allocation group's free space, read once and then edited in
/// memory, so that several allocations in one operation see each other.
///
/// ONE OPERATION CAN ALLOCATE TWICE. Creating a file in a group whose
/// inode chunks are all full needs blocks for a new chunk, and if the
/// parent's short-form directory overflows at the same moment it needs
/// blocks for the directory's first block as well. A journalled
/// operation writes nothing until its record is written, so an
/// allocator that re-reads the group from the device cannot see what an
/// earlier one in the same operation took: both pick the same run. The
/// record then carries two AGF items and two free-space tree items,
/// each diffed against the same before-image, so recovery applies both
/// and the last one wins -- the chunk and the directory block get the
/// same blocks, and the trees still list one of them as free. No
/// hostile image is needed; an ordinary filesystem does it.
///
/// Reading once and taking twice is what makes the second take see the
/// first, and emitting the items once at the end is what stops two
/// diffs of the same buffer from reaching the log.
pub(crate) struct GroupAlloc<'a> {
    sb: &'a Superblock,
    device: &'a dyn fs_core::BlockRead,
    agno: u32,
    /// Byte offset of the group on the device.
    ag_start: u64,
    /// The group header as it was, which every change is diffed
    /// against.
    agf_raw: Vec<u8>,
    /// Every block of every tree read here, as it was. A tree laid out
    /// again writes over the blocks it already had, and a block whose
    /// bytes come out the same is not logged at all.
    before: std::collections::HashMap<u32, Vec<u8>>,
    /// The blocks each tree occupies, leaves first and root last --
    /// the order [`crate::ag_btree::build`] wants them in.
    bno_blocks: Vec<u32>,
    cnt_blocks: Vec<u32>,
    /// Free space in block order, as it stands after the takes so far.
    by_block: Vec<FreeExtent>,
    /// The reverse-mapping tree's records and blocks, where the
    /// filesystem has that tree.
    rmap: Option<(Vec<crate::rmap::Rmap>, Vec<u32>)>,
    /// The group's free list: where a growing tree gets a block and
    /// where a shrinking one puts it back.
    agfl: crate::agfl::Agfl,
    agfl_raw: Vec<u8>,
    /// The reference-count tree's records and blocks, where the
    /// filesystem has reflink and the tree exists. A reflink filesystem
    /// that has never shared anything has the feature and no tree.
    refcount: Option<(Vec<crate::refcount::Refcount>, Vec<u32>)>,
    /// Whether anything has actually been taken. Nothing taken means
    /// nothing to log, rather than items whose diffs are empty.
    took: bool,
}

impl<'a> GroupAlloc<'a> {
    /// Read the group's header, its free list and its trees.
    ///
    /// The trees are read whole, however deep they are: a group in use
    /// has more free runs than one block holds, and the edit below lays
    /// the tree out again rather than reaching into it.
    ///
    /// # Errors
    ///
    /// Whatever reading the group's trees returns, and
    /// [`Error::BadSuperblock`] for a free list whose header does not
    /// describe the blocks it holds.
    pub(crate) fn open(
        sb: &'a Superblock,
        device: &'a dyn fs_core::BlockRead,
        agno: u32,
    ) -> Result<Self> {
        use crate::ag::agf_btree::{BNO, CNT, RMAP};
        use crate::ag::Agf;

        let blocksize = u64::from(sb.blocksize);
        let sector = u64::from(sb.sectsize);
        let ag_start = u64::from(agno) * u64::from(sb.agblocks) * blocksize;

        let mut agf_raw = vec![0u8; sb.sectsize as usize];
        device.read_at(ag_start + sector, &mut agf_raw)?;
        let agf = Agf::parse(&agf_raw, sb, agno)?;

        // THE FREE LIST. A tree that grows takes a block from here and
        // one that shrinks puts it back, because the block cannot come
        // out of the free-space tree -- taking it is the edit that
        // needed it. Its header has to describe the blocks it holds
        // before anything here edits the group: a count that disagrees
        // with the indices means the group's own accounting is already
        // wrong, and laying its trees out again on top of that would
        // bury the evidence.
        let mut agfl_raw = vec![0u8; sb.sectsize as usize];
        device.read_at(ag_start + sector * 3, &mut agfl_raw)?;
        let agfl = crate::agfl::Agfl::parse(&agfl_raw, sb, &agf, agno)?;

        let mut before: std::collections::HashMap<u32, Vec<u8>> = std::collections::HashMap::new();

        // Reading through the walker records every block as it goes, so
        // the before-images cost nothing beyond the read the walk was
        // making anyway.
        let mut read = |agblock: u32| -> Result<Vec<u8>> {
            let mut raw = vec![0u8; sb.blocksize as usize];
            device.read_at(ag_start + u64::from(agblock) * blocksize, &mut raw)?;
            Ok(raw)
        };

        let (by_block, bno_blocks) = crate::ag_btree::walk_blocks(
            sb,
            crate::alloc_btree::Order::ByBlock.shape(),
            agno,
            agf.roots[BNO],
            agf.levels[BNO],
            &mut read,
            crate::alloc_btree::decode_free_extent,
        )?;
        // The by-length tree holds the same records in another order, so
        // only its blocks are wanted -- but they have to be read to be
        // known, and their contents are what the new tree is diffed
        // against.
        let (_, cnt_blocks) = crate::ag_btree::walk_blocks(
            sb,
            crate::alloc_btree::Order::ByCount.shape(),
            agno,
            agf.roots[CNT],
            agf.levels[CNT],
            &mut read,
            crate::alloc_btree::decode_free_extent,
        )?;

        // A FILESYSTEM CAN HAVE THE FEATURE AND A GROUP NO TREE.
        //
        // `has_rmapbt` is the superblock's, and the group's own header
        // is what says whether this group has the tree yet. Reading the
        // feature and then trusting `roots[RMAP]` walks whatever block
        // zero happens to be -- which is the superblock, and the error
        // says "rmapbt block 0 has magic XFSB".
        let rmap = if sb.has_rmapbt() && agf.levels[RMAP] > 0 {
            let (records, blocks) = crate::ag_btree::walk_blocks(
                sb,
                crate::rmap::shape(),
                agno,
                agf.roots[RMAP],
                agf.levels[RMAP],
                &mut read,
                crate::rmap::decode,
            )?;
            Some((records, blocks))
        } else {
            None
        };

        let refcount = if sb.has_reflink() && agf.refcount_level > 0 {
            let (records, blocks) = crate::ag_btree::walk_blocks(
                sb,
                crate::refcount::shape(),
                agno,
                agf.refcount_root,
                agf.refcount_level,
                &mut read,
                crate::refcount::decode,
            )?;
            Some((records, blocks))
        } else {
            None
        };

        for &agblock in bno_blocks
            .iter()
            .chain(cnt_blocks.iter())
            .chain(rmap.iter().flat_map(|(_, b)| b.iter()))
            .chain(refcount.iter().flat_map(|(_, b)| b.iter()))
        {
            let mut raw = vec![0u8; sb.blocksize as usize];
            device.read_at(ag_start + u64::from(agblock) * blocksize, &mut raw)?;
            before.insert(agblock, raw);
        }

        Ok(GroupAlloc {
            sb,
            device,
            agno,
            ag_start,
            agf_raw,
            before,
            bno_blocks,
            cnt_blocks,
            by_block,
            rmap,
            refcount,
            agfl,
            agfl_raw,
            took: false,
        })
    }

    /// Which group this is taking from.
    pub(crate) fn agno(&self) -> u32 {
        self.agno
    }

    /// Take `want` contiguous blocks for `owner` at file offset
    /// `offset`, and say where they start.
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
    /// [`Error::UnsupportedFeature`] when no single run is long enough.
    pub(crate) fn take(&mut self, want: u32, owner: i64, offset: u64) -> Result<u32> {
        use crate::alloc_btree::{alloc_extent, longest};

        let agno = self.agno;
        let chosen = self
            .by_block
            .iter()
            .find(|run| run.blockcount >= want)
            .copied()
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "allocation group {agno} has no single free run of {want} blocks — its \
                     longest is {}, and splitting across extents is not implemented",
                    longest(&self.by_block)
                ))
            })?;
        let taking = FreeExtent {
            startblock: chosen.startblock,
            blockcount: want,
        };
        alloc_extent(&mut self.by_block, taking)?;

        // The reverse map, where the filesystem has one. Blocks that
        // have just left free space belong to `owner` from here on, and
        // a tree that does not say so describes a filesystem where they
        // belong to nobody.
        if let Some((records, _)) = self.rmap.as_mut() {
            crate::rmap::insert(
                records,
                crate::rmap::Rmap {
                    startblock: taking.startblock,
                    blockcount: taking.blockcount,
                    owner,
                    offset,
                },
            )?;
        }

        self.took = true;
        Ok(taking.startblock)
    }

    /// Give an extent back to free space, merging it with whatever it
    /// adjoins.
    ///
    /// The counterpart of [`GroupAlloc::take`], and the reason both live
    /// on the same type: an operation that frees in one group and
    /// allocates in it -- a truncate that returns blocks and a directory
    /// that grows in the same record -- has to see one free list, not
    /// two readings of the same one.
    pub(crate) fn give_back(&mut self, extent: FreeExtent) -> Result<crate::alloc_btree::Freed> {
        let freed = crate::alloc_btree::free_extent(&mut self.by_block, extent)?;
        self.took = true;
        Ok(freed)
    }

    /// Take an extent's ownership record out of the reverse map.
    ///
    /// Matched exactly: a record that does not line up with the extent
    /// means the tree and the inode disagree, and the free must not go
    /// ahead on top of that. Does nothing where the filesystem has no
    /// reverse-mapping tree.
    pub(crate) fn forget_rmap(&mut self, record: crate::rmap::Rmap) -> Result<()> {
        if let Some((records, _)) = self.rmap.as_mut() {
            crate::rmap::remove(records, record)?;
            self.took = true;
        }
        Ok(())
    }

    /// Give up one reference to `startblock..+blockcount`, and say which
    /// of those blocks may go back to free space.
    ///
    /// Everything on a filesystem without the reference-count tree, and
    /// on one with it, whatever [`crate::refcount::release`] decides --
    /// which is never the blocks another file still holds.
    pub(crate) fn release_shared(
        &mut self,
        startblock: u32,
        blockcount: u32,
    ) -> Result<Vec<FreeExtent>> {
        let Some((records, _)) = self.refcount.as_mut() else {
            return Ok(vec![FreeExtent {
                startblock,
                blockcount,
            }]);
        };
        let before = records.clone();
        let ranges = crate::refcount::release(records, startblock, blockcount)?;
        if *records != before {
            self.took = true;
        }
        Ok(ranges)
    }

    /// Lay one tree out again, through the shared layout.
    fn relay<T, E, K>(
        &mut self,
        shape: crate::ag_btree::Shape,
        records: &[T],
        held: &[u32],
        encode_record: E,
        write_keys: K,
        items: &mut Vec<BufferItem>,
    ) -> Result<Vec<u32>>
    where
        E: Fn(&mut [u8], usize, &T),
        K: Fn(&mut [u8], usize, &[T]),
    {
        crate::ag_btree::relay(
            self.sb,
            self.device,
            self.ag_start,
            shape,
            self.agno,
            records,
            held,
            &self.before,
            &mut self.agfl,
            encode_record,
            write_keys,
            items,
        )
    }

    /// The buffer items recording everything taken.
    ///
    /// One item per buffer however many takes there were: the group
    /// header, the free list where it moved, and every block of every
    /// tree whose bytes changed. Nothing is written; the items are the
    /// change, and the caller puts them in a record.
    pub(crate) fn into_items(mut self) -> Result<Vec<BufferItem>> {
        use crate::ag::agf_btree::{BNO, CNT, RMAP};
        use crate::ag::offsets::agf;
        use crate::alloc_btree::{longest, total_free};
        use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGF, BLFT_AGFL};
        use crate::log::BBSIZE;

        if !self.took {
            return Ok(Vec::new());
        }

        let agno = self.agno;
        let sector = u64::from(self.sb.sectsize);
        let mut items = Vec::new();

        let by_block = std::mem::take(&mut self.by_block);
        let mut by_count = by_block.clone();
        by_count.sort_by_key(|e| (e.blockcount, e.startblock));

        let held = std::mem::take(&mut self.bno_blocks);
        let bno_blocks = self.relay(
            crate::alloc_btree::Order::ByBlock.shape(),
            &by_block,
            &held,
            crate::alloc_btree::encode_free_extent,
            |buf, at, recs: &[FreeExtent]| {
                crate::alloc_btree::encode_free_extent(buf, at, &recs[0])
            },
            &mut items,
        )?;

        let held = std::mem::take(&mut self.cnt_blocks);
        let cnt_blocks = self.relay(
            crate::alloc_btree::Order::ByCount.shape(),
            &by_count,
            &held,
            crate::alloc_btree::encode_free_extent,
            |buf, at, recs: &[FreeExtent]| {
                crate::alloc_btree::encode_free_extent(buf, at, &recs[0])
            },
            &mut items,
        )?;

        let (rmap_blocks, rmap_records) = match self.rmap.take() {
            None => (Vec::new(), 0),
            Some((records, held)) => {
                let count = records.len();
                let blocks = self.relay(
                    crate::rmap::shape(),
                    &records,
                    &held,
                    crate::rmap::encode,
                    crate::rmap::write_keys,
                    &mut items,
                )?;
                (blocks, count)
            }
        };

        let (refcount_blocks, refcount_records) = match self.refcount.take() {
            None => (Vec::new(), 0),
            Some((records, held)) => {
                let count = records.len();
                let blocks = self.relay(
                    crate::refcount::shape(),
                    &records,
                    &held,
                    crate::refcount::encode,
                    |buf, at, recs: &[crate::refcount::Refcount]| {
                        crate::refcount::encode_key(buf, at, &recs[0])
                    },
                    &mut items,
                )?;
                (blocks, count)
            }
        };

        // THE HEADER LAST, because it describes what the trees came out
        // as rather than what they were asked for.
        let mut new_agf = self.agf_raw.clone();
        let freeblks = u32::try_from(total_free(&by_block)).map_err(|_| {
            Error::CorruptLog(format!(
                "allocation group {agno} has more free blocks than fit"
            ))
        })?;
        new_agf[agf::FREEBLKS..agf::FREEBLKS + 4].copy_from_slice(&freeblks.to_be_bytes());
        new_agf[agf::LONGEST..agf::LONGEST + 4].copy_from_slice(&longest(&by_block).to_be_bytes());

        let root_of = |blocks: &[u32]| *blocks.last().expect("a tree has at least one block");
        let level_of =
            |blocks: &[u32], shape: crate::ag_btree::Shape, records: usize| -> Result<u32> {
                let plan =
                    crate::ag_btree::plan(shape, self.sb.blocksize, self.sb.is_v5(), records)?;
                // THE DEPTH STAMPED INTO agf_levels COMES FROM THIS PLAN.
                // If a re-derived plan disagrees with the blocks the tree
                // was actually laid out over, the group header records a
                // depth that does not match the disk. `debug_assert_eq!`
                // never ran: nothing here builds in debug.
                if plan.iter().sum::<usize>() != blocks.len() {
                    return Err(Error::Internal(format!(
                        "a re-derived plan covers {} blocks but the tree was laid out \
                         over {}",
                        plan.iter().sum::<usize>(),
                        blocks.len()
                    )));
                }
                Ok(plan.len() as u32)
            };

        let bno_level = level_of(
            &bno_blocks,
            crate::alloc_btree::Order::ByBlock.shape(),
            by_block.len(),
        )?;
        let cnt_level = level_of(
            &cnt_blocks,
            crate::alloc_btree::Order::ByCount.shape(),
            by_count.len(),
        )?;
        let put = |buf: &mut [u8], at: usize, v: u32| {
            buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
        };
        put(&mut new_agf, agf::ROOTS + BNO * 4, root_of(&bno_blocks));
        put(&mut new_agf, agf::ROOTS + CNT * 4, root_of(&cnt_blocks));
        put(&mut new_agf, agf::LEVELS + BNO * 4, bno_level);
        put(&mut new_agf, agf::LEVELS + CNT * 4, cnt_level);

        // MEASURED, on a fixture whose three trees are all two levels
        // deep: `btreeblks` 21 with a by-block tree of 4 blocks, a
        // by-length tree of 4 and a reverse-mapping tree of 16 --
        // (4-1) + (4-1) + (16-1). It counts what the trees hold BELOW
        // their roots, across all three. `rmap_blocks` counts that tree
        // whole, root included: 16.
        let mut btreeblks = (bno_blocks.len() - 1) + (cnt_blocks.len() - 1);
        if !rmap_blocks.is_empty() {
            btreeblks += rmap_blocks.len() - 1;
            put(&mut new_agf, agf::ROOTS + RMAP * 4, root_of(&rmap_blocks));
            put(
                &mut new_agf,
                agf::LEVELS + RMAP * 4,
                level_of(&rmap_blocks, crate::rmap::shape(), rmap_records)?,
            );
            put(&mut new_agf, agf::RMAP_BLOCKS, rmap_blocks.len() as u32);
        }
        // The reference-count tree keeps its own count and its own
        // root, and is NOT part of `btreeblks` -- that field is the
        // free-space and reverse-mapping trees, which is what the
        // measurement above covers.
        if !refcount_blocks.is_empty() {
            put(&mut new_agf, agf::REFCOUNT_ROOT, root_of(&refcount_blocks));
            put(
                &mut new_agf,
                agf::REFCOUNT_LEVEL,
                level_of(&refcount_blocks, crate::refcount::shape(), refcount_records)?,
            );
            put(
                &mut new_agf,
                agf::REFCOUNT_BLOCKS,
                refcount_blocks.len() as u32,
            );
        }

        put(
            &mut new_agf,
            agf::BTREEBLKS,
            u32::try_from(btreeblks).map_err(|_| {
                Error::CorruptLog(format!(
                    "allocation group {agno}'s trees hold more blocks than fit"
                ))
            })?,
        );

        // The free list, which the trees may have taken from or given
        // back to.
        put(&mut new_agf, agf::FLFIRST, self.agfl.first());
        put(&mut new_agf, agf::FLLAST, self.agfl.last());
        put(&mut new_agf, agf::FLCOUNT, self.agfl.count());
        // The checksum is left stale on purpose — recovery recomputes it.

        let ag_bb = self.ag_start / BBSIZE as u64;
        items.insert(
            0,
            changed_chunks(
                ag_bb + sector / BBSIZE as u64,
                &self.agf_raw,
                new_agf,
                BLFT_AGF,
            ),
        );

        let after = self.agfl.after(self.sb);
        if after != self.agfl_raw {
            items.push(changed_chunks(
                ag_bb + sector * 3 / BBSIZE as u64,
                &self.agfl_raw,
                after,
                BLFT_AGFL,
            ));
        }

        Ok(items)
    }
}

/// The groups one operation has taken blocks from.
///
/// An operation allocates in at most a couple of groups, and it has to
/// hold each one open across every take it makes there -- see
/// [`GroupAlloc`] for what happens when it does not. Keyed by group so
/// that two takes in the same group share a [`GroupAlloc`] and two in
/// different groups do not.
pub(crate) struct Allocations<'a> {
    groups: Vec<GroupAlloc<'a>>,
}

impl<'a> Allocations<'a> {
    pub(crate) fn new() -> Self {
        Allocations { groups: Vec::new() }
    }

    /// The open allocator for `agno`, opening it if this is the first
    /// take there.
    pub(crate) fn group(
        &mut self,
        sb: &'a Superblock,
        device: &'a dyn fs_core::BlockRead,
        agno: u32,
    ) -> Result<&mut GroupAlloc<'a>> {
        if let Some(i) = self.groups.iter().position(|g| g.agno() == agno) {
            return Ok(&mut self.groups[i]);
        }
        self.groups.push(GroupAlloc::open(sb, device, agno)?);
        Ok(self.groups.last_mut().expect("just pushed"))
    }

    /// Every item, group by group in group order, so a record built
    /// twice from the same takes is the same record.
    pub(crate) fn into_items(mut self) -> Result<Vec<BufferItem>> {
        self.groups.sort_by_key(GroupAlloc::agno);
        let mut items = Vec::new();
        for group in self.groups {
            items.extend(group.into_items()?);
        }
        Ok(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------
    // The invariants run in the build that ships
    // -----------------------------------------------------------------
    //
    // `changed_chunks` is the one converted invariant reachable from
    // outside: both slices come from the caller, so a mismatch needs no
    // fault injection to produce. The rest of them guard a disagreement
    // between two internal computations -- `ag_btree::build` already
    // refuses a caller-supplied block count upstream, with "needs N
    // blocks" -- and cannot be reached through any public argument.
    //
    // This one is worth having because it is a genuine witness, and the
    // lengths in it are chosen so that it is. The loop bounds come from
    // `before`, while the item is built over `after`, so a LONGER
    // `after` is the silent direction: nothing indexes out of range, and
    // `changed_chunks` returns an item covering bytes it never examined.
    // Every change in that unexamined tail is left unmarked and so never
    // reaches the journal at all.
    //
    // Both lengths are whole basic blocks on purpose. A mismatch of
    // 512 against 256 does fail on `main`, but by tripping
    // `BufferItem::new`'s own "whole number of basic blocks" assert one
    // frame later -- a downstream panic about the wrong thing, not the
    // silent case. Testing against that would have proved less than it
    // appeared to.

    /// A diff of two different lengths is not a diff.
    ///
    /// `#[should_panic]` and not an `Err`, because the function returns
    /// `BufferItem` rather than `Result` and a refusal therefore has to
    /// be a panic. The point of the test is the build it runs in: this
    /// suite is `--release` throughout, which is exactly where the
    /// previous `debug_assert_eq!` had been compiled out.
    #[test]
    #[should_panic(expected = "compares 512 bytes against 1024")]
    fn a_buffer_diff_of_mismatched_lengths_is_refused() {
        let before = vec![0u8; 512];
        let after = vec![0xffu8; 1024];
        let _ = changed_chunks(1, &before, after, 0);
    }

    /// And the equal-length case still works, so the test above is
    /// failing on the mismatch rather than on anything else in the call.
    #[test]
    fn a_buffer_diff_of_equal_lengths_still_describes_the_change() {
        let before = vec![0u8; 512];

        // Self-calibrating rather than a fixed count: an unchanged
        // buffer establishes what "nothing logged" looks like, so the
        // changed case cannot pass by the function returning something
        // for every input.
        let unchanged = changed_chunks(1, &before, before.clone(), 0);

        let mut after = before.clone();
        after[0] = 0xff;
        let changed = changed_chunks(1, &before, after, 0);

        assert_eq!(changed.data().len(), 512);
        assert!(
            changed.op_count() > unchanged.op_count(),
            "a changed byte logged {} regions, the same as an unchanged buffer",
            changed.op_count()
        );
    }

    // -----------------------------------------------------------------
    // Two allocations in one operation
    // -----------------------------------------------------------------

    mod one_operation {
        use super::*;
        use crate::superblock::crc32c_with_zeroed_crc;
        use std::sync::Mutex;

        const BLOCKSIZE: u32 = 4096;
        const SECTSIZE: u16 = 512;
        const AGBLOCKS: u32 = 1000;
        const BNO_ROOT: u32 = 1;
        const CNT_ROOT: u32 = 2;
        /// The one free run this group has: 500 blocks from block 100.
        const FREE_START: u32 = 100;
        const FREE_LEN: u32 = 500;

        struct MemDev(Mutex<Vec<u8>>);

        impl fs_core::BlockRead for MemDev {
            fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
                let b = self.0.lock().unwrap();
                let start = offset as usize;
                let end = start + buf.len();
                assert!(end <= b.len(), "read past the end of the device");
                buf.copy_from_slice(&b[start..end]);
                Ok(())
            }
            fn size_bytes(&self) -> u64 {
                self.0.lock().unwrap().len() as u64
            }
        }

        fn superblock() -> Superblock {
            let mut b = vec![0u8; SECTSIZE as usize];
            b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
            b[4..8].copy_from_slice(&BLOCKSIZE.to_be_bytes());
            b[8..16].copy_from_slice(&u64::from(AGBLOCKS).to_be_bytes()); // dblocks
            for (i, slot) in b[32..48].iter_mut().enumerate() {
                *slot = i as u8;
            }
            b[48..56].copy_from_slice(&600u64.to_be_bytes()); // logstart
            b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
            b[84..88].copy_from_slice(&AGBLOCKS.to_be_bytes());
            b[88..92].copy_from_slice(&1u32.to_be_bytes()); // agcount
            let versionnum = 5u16 | crate::superblock::version_flags::MOREBITSBIT;
            b[100..102].copy_from_slice(&versionnum.to_be_bytes());
            b[102..104].copy_from_slice(&SECTSIZE.to_be_bytes());
            b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
            b[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
            b[120] = 12; // blocklog
            b[121] = 9; // sectlog
            b[122] = 9; // inodelog
            b[123] = 3; // inopblog
            b[124] = 10; // agblklog
            let crc = crc32c_with_zeroed_crc(&b, 224);
            b[224..228].copy_from_slice(&crc.to_le_bytes());
            Superblock::parse(&b).expect("v5 superblock")
        }

        /// A group with one free run, no reverse-mapping tree, and both
        /// free-space trees a single leaf.
        fn image(sb: &Superblock) -> MemDev {
            use crate::ag::offsets::{agf, common};
            let mut dev = vec![0u8; (AGBLOCKS as usize) * BLOCKSIZE as usize];

            let mut agf_raw = vec![0u8; SECTSIZE as usize];
            agf_raw[common::MAGIC..common::MAGIC + 4]
                .copy_from_slice(&crate::ag::XFS_AGF_MAGIC.to_be_bytes());
            agf_raw[common::VERSIONNUM..common::VERSIONNUM + 4]
                .copy_from_slice(&1u32.to_be_bytes());
            agf_raw[common::SEQNO..common::SEQNO + 4].copy_from_slice(&0u32.to_be_bytes());
            agf_raw[common::LENGTH..common::LENGTH + 4].copy_from_slice(&AGBLOCKS.to_be_bytes());
            agf_raw[agf::ROOTS..agf::ROOTS + 4].copy_from_slice(&BNO_ROOT.to_be_bytes());
            agf_raw[agf::ROOTS + 4..agf::ROOTS + 8].copy_from_slice(&CNT_ROOT.to_be_bytes());
            agf_raw[agf::LEVELS..agf::LEVELS + 4].copy_from_slice(&1u32.to_be_bytes());
            agf_raw[agf::LEVELS + 4..agf::LEVELS + 8].copy_from_slice(&1u32.to_be_bytes());
            agf_raw[agf::FREEBLKS..agf::FREEBLKS + 4].copy_from_slice(&FREE_LEN.to_be_bytes());
            agf_raw[agf::LONGEST..agf::LONGEST + 4].copy_from_slice(&FREE_LEN.to_be_bytes());
            agf_raw[agf::UUID..agf::UUID + 16].copy_from_slice(&sb.meta_uuid);
            let crc = crc32c_with_zeroed_crc(&agf_raw, agf::CRC);
            agf_raw[agf::CRC..agf::CRC + 4].copy_from_slice(&crc.to_le_bytes());
            let at = SECTSIZE as usize;
            dev[at..at + agf_raw.len()].copy_from_slice(&agf_raw);

            // THE FREE LIST, empty but present. A group without one is
            // not a group: the editor checks that the list's header
            // describes the blocks it holds before it changes anything,
            // and a fixture missing the sector would be exercising the
            // editor against a filesystem that could not exist.
            let mut agfl_raw = vec![0u8; SECTSIZE as usize];
            agfl_raw[crate::agfl::offsets::MAGIC..crate::agfl::offsets::MAGIC + 4]
                .copy_from_slice(&crate::ag::XFS_AGFL_MAGIC.to_be_bytes());
            agfl_raw[crate::agfl::offsets::SEQNO..crate::agfl::offsets::SEQNO + 4]
                .copy_from_slice(&0u32.to_be_bytes());
            agfl_raw[crate::agfl::offsets::UUID..crate::agfl::offsets::UUID + 16]
                .copy_from_slice(&sb.meta_uuid);
            let crc = crc32c_with_zeroed_crc(&agfl_raw, crate::agfl::offsets::CRC);
            agfl_raw[crate::agfl::offsets::CRC..crate::agfl::offsets::CRC + 4]
                .copy_from_slice(&crc.to_le_bytes());
            let at = SECTSIZE as usize * 3;
            dev[at..at + agfl_raw.len()].copy_from_slice(&agfl_raw);

            let free = vec![FreeExtent {
                startblock: FREE_START,
                blockcount: FREE_LEN,
            }];
            for (root, order) in [
                (BNO_ROOT, crate::alloc_btree::Order::ByBlock),
                (CNT_ROOT, crate::alloc_btree::Order::ByCount),
            ] {
                let mut block = vec![0u8; BLOCKSIZE as usize];
                // The v5 magics, spelled as they appear in a hex dump.
                let magic: &[u8; 4] = match order {
                    crate::alloc_btree::Order::ByBlock => b"AB3B",
                    crate::alloc_btree::Order::ByCount => b"AB3C",
                };
                block[0..4].copy_from_slice(magic);
                block[4..6].copy_from_slice(&0u16.to_be_bytes()); // level
                block[6..8].copy_from_slice(&1u16.to_be_bytes()); // numrecs
                let blkno = crate::alloc_btree::expected_blkno(sb, 0, root);
                block[16..24].copy_from_slice(&blkno.to_be_bytes());
                block[32..48].copy_from_slice(&sb.meta_uuid);
                block[48..52].copy_from_slice(&0u32.to_be_bytes()); // owner
                let mut filled = rebuild_leaf(&block, &free);
                // CHECKSUMMED, because the editor reads these through
                // the same walker the driver reads a real filesystem
                // with, and that walker checks. A fixture the reader
                // would refuse is a fixture that proves nothing about
                // the writer.
                let crc = crc32c_with_zeroed_crc(&filled, crate::ag_btree::offsets::CRC);
                filled[crate::ag_btree::offsets::CRC..crate::ag_btree::offsets::CRC + 4]
                    .copy_from_slice(&crc.to_le_bytes());
                let at = root as usize * BLOCKSIZE as usize;
                dev[at..at + filled.len()].copy_from_slice(&filled);
            }

            MemDev(Mutex::new(dev))
        }

        /// THE DEFECT, at the smallest scale that shows it.
        ///
        /// One create can allocate twice: a new inode chunk when the
        /// group has no free inode, and the block a short-form directory
        /// moves into when it overflows. Nothing is on disk until the
        /// record is written, so an allocator that re-reads the group
        /// from the device sees the free space the first take had
        /// already spent, and hands out the same blocks again.
        #[test]
        fn a_second_take_does_not_get_the_first_ones_blocks() {
            let sb = superblock();
            let dev = image(&sb);

            let mut alloc = GroupAlloc::open(&sb, &dev, 0).expect("the group opens");
            let chunk = alloc
                .take(8, crate::rmap::OWN_INODES, 0)
                .expect("first take");
            let dir_block = alloc.take(1, 131, 0).expect("second take");

            assert_eq!(chunk, FREE_START, "the first take gets the run's start");
            assert_eq!(
                dir_block,
                FREE_START + 8,
                "the second take must start where the first ended, not at {chunk}"
            );
        }

        /// One buffer, one item, however many takes -- because a buffer
        /// item is a diff against a before-image, and two diffs of the
        /// same buffer against the same before-image do not compose:
        /// recovery applies both and the last one wins.
        #[test]
        fn two_takes_log_each_buffer_once() {
            let sb = superblock();
            let dev = image(&sb);

            let mut alloc = GroupAlloc::open(&sb, &dev, 0).expect("the group opens");
            alloc
                .take(8, crate::rmap::OWN_INODES, 0)
                .expect("first take");
            alloc.take(1, 131, 0).expect("second take");
            let items = alloc.into_items().expect("items");

            assert_eq!(
                items.len(),
                3,
                "the group header and its two free-space trees, once each"
            );
        }

        /// Taking nothing logs nothing, rather than three items whose
        /// diffs are empty.
        #[test]
        fn a_group_nothing_was_taken_from_logs_nothing() {
            let sb = superblock();
            let dev = image(&sb);
            let alloc = GroupAlloc::open(&sb, &dev, 0).expect("the group opens");
            assert!(alloc.into_items().expect("items").is_empty());
        }
    }
    use crate::format::log_items::buf_log_format::buf_type::BLFT_BTREE;

    /// Only the chunks that differ are logged, and a byte written back
    /// to what it already was is not a change.
    /// `bb_numrecs` is two bytes off a block this driver verifies in no
    /// other way -- no magic, no CRC, no owner, no self-address, unlike
    /// `alloc_btree::parse_block`, which checks all four. It was then a
    /// loop bound over `buf[at..at + RECORD]`, so 0xFFFF at a 4 KiB
    /// block size indexed past the end and panicked out of
    /// `free_extents`, `rmap_records` and `refcount_records` alike.
    #[test]
    fn a_root_may_not_claim_more_records_than_fit_in_it() {
        let mut buf = vec![0u8; 4096];
        let fits = (4096 - btree::V5_BODY) / btree::RECORD; // 505
        let put = |buf: &mut [u8], n: u16| {
            buf[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&n.to_be_bytes());
        };

        put(&mut buf, fits as u16);
        assert_eq!(leaf_numrecs(&buf, btree::RECORD).unwrap(), fits as u16);

        put(&mut buf, fits as u16 + 1);
        assert!(leaf_numrecs(&buf, btree::RECORD).is_err());

        put(&mut buf, 0xFFFF);
        assert!(leaf_numrecs(&buf, btree::RECORD).is_err());

        // A wider record leaves room for fewer of them: 168 reverse
        // mappings, 336 reference counts.
        put(&mut buf, 169);
        assert!(leaf_numrecs(&buf, crate::rmap::RECORD).is_err());
        put(&mut buf, 168);
        assert!(leaf_numrecs(&buf, crate::rmap::RECORD).is_ok());

        // And the records themselves stop where the block does, so a
        // count that slipped past the check cannot index off the end.
        put(&mut buf, 0xFFFF);
        assert_eq!(leaf_records(&buf, 0xFFFF).len(), fits);
    }

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
