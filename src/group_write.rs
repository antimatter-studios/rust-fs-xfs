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
    agno: u32,
    /// Byte offset of the group on the device.
    ag_start: u64,
    /// The group header and the tree roots as they were before any of
    /// this, which is what every change is diffed against.
    agf_raw: Vec<u8>,
    agf: crate::ag::Agf,
    bno_raw: Vec<u8>,
    cnt_raw: Vec<u8>,
    /// Free space in block order, as it stands after the takes so far.
    by_block: Vec<FreeExtent>,
    /// The reverse-mapping root and its records, where the filesystem
    /// has that tree.
    rmap: Option<(Vec<u8>, Vec<crate::rmap::Rmap>)>,
    /// Whether anything has actually been taken. Nothing taken means
    /// nothing to log, rather than four items whose diffs are empty.
    took: bool,
}

impl<'a> GroupAlloc<'a> {
    /// Read the group's header and trees, and check that all of them
    /// are shapes this can maintain.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when any of the trees is more than
    /// one level deep, where taking a record out can collapse a node.
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

        let mut bno_raw = vec![0u8; sb.blocksize as usize];
        device.read_at(
            ag_start + u64::from(agf.roots[BNO]) * blocksize,
            &mut bno_raw,
        )?;
        let mut cnt_raw = vec![0u8; sb.blocksize as usize];
        device.read_at(
            ag_start + u64::from(agf.roots[CNT]) * blocksize,
            &mut cnt_raw,
        )?;

        let numrecs = leaf_numrecs(&bno_raw, btree::RECORD)?;
        let by_block = leaf_records(&bno_raw, numrecs);

        let rmap = if sb.has_rmapbt() {
            if agf.levels[RMAP] != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno}'s reverse-mapping tree is {} levels deep, \
                     where inserting a record can split a node; only a single-level tree \
                     is supported",
                    agf.levels[RMAP]
                )));
            }
            let mut rmap_raw = vec![0u8; sb.blocksize as usize];
            device.read_at(
                ag_start + u64::from(agf.roots[RMAP]) * blocksize,
                &mut rmap_raw,
            )?;
            let n = leaf_numrecs(&rmap_raw, crate::rmap::RECORD)?;
            let records = crate::rmap::leaf_records(&rmap_raw, n);
            Some((rmap_raw, records))
        } else {
            None
        };

        Ok(GroupAlloc {
            sb,
            agno,
            ag_start,
            agf_raw,
            agf,
            bno_raw,
            cnt_raw,
            by_block,
            rmap,
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
    /// [`Error::UnsupportedFeature`] when no single run is long enough,
    /// or when the result would need more records than a tree root
    /// holds.
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

        let capacity = leaf_capacity(self.sb.blocksize);
        if self.by_block.len() > capacity {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno} would need {} free-space records and its tree root \
                 holds {capacity}; splitting a node is not implemented",
                self.by_block.len()
            )));
        }

        // The reverse map, where the filesystem has one. Blocks that
        // have just left free space belong to `owner` from here on, and
        // a tree that does not say so describes a filesystem where they
        // belong to nobody.
        if let Some((_, records)) = self.rmap.as_mut() {
            crate::rmap::insert(
                records,
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
        }

        self.took = true;
        Ok(taking.startblock)
    }

    /// The buffer items recording everything taken.
    ///
    /// One item per buffer however many takes there were: the group
    /// header, the by-block tree, the by-length tree and — where the
    /// filesystem has one — the reverse-mapping tree, in that order.
    /// Nothing is written; the items are the change, and the caller
    /// puts them in a record.
    pub(crate) fn into_items(self) -> Result<Vec<BufferItem>> {
        use crate::ag::agf_btree::{BNO, CNT, RMAP};
        use crate::alloc_btree::{expected_blkno, longest, total_free};
        use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGF, BLFT_BTREE};
        use crate::log::BBSIZE;

        if !self.took {
            return Ok(Vec::new());
        }

        let sb = self.sb;
        let agno = self.agno;
        let sector = u64::from(sb.sectsize);

        let mut by_count = self.by_block.clone();
        by_count.sort_by_key(|e| (e.blockcount, e.startblock));

        let new_bno = rebuild_leaf(&self.bno_raw, &self.by_block);
        let new_cnt = rebuild_leaf(&self.cnt_raw, &by_count);

        let mut new_agf = self.agf_raw.clone();
        let freeblks = u32::try_from(total_free(&self.by_block)).map_err(|_| {
            Error::CorruptLog(format!(
                "allocation group {agno} has more free blocks than fit"
            ))
        })?;
        new_agf[agf::FREEBLKS..agf::FREEBLKS + 4].copy_from_slice(&freeblks.to_be_bytes());
        new_agf[agf::LONGEST..agf::LONGEST + 4]
            .copy_from_slice(&longest(&self.by_block).to_be_bytes());
        // The checksum is left stale on purpose — recovery recomputes it.

        let ag_bb = self.ag_start / BBSIZE as u64;
        let mut items = vec![
            changed_chunks(
                ag_bb + sector / BBSIZE as u64,
                &self.agf_raw,
                new_agf,
                BLFT_AGF,
            ),
            changed_chunks(
                expected_blkno(sb, agno, self.agf.roots[BNO]),
                &self.bno_raw,
                new_bno,
                BLFT_BTREE,
            ),
            changed_chunks(
                expected_blkno(sb, agno, self.agf.roots[CNT]),
                &self.cnt_raw,
                new_cnt,
                BLFT_BTREE,
            ),
        ];

        if let Some((rmap_raw, records)) = self.rmap {
            let new_rmap = crate::rmap::rebuild_leaf(&rmap_raw, &records);
            items.push(changed_chunks(
                expected_blkno(sb, agno, self.agf.roots[RMAP]),
                &rmap_raw,
                new_rmap,
                BLFT_BTREE,
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
                let filled = rebuild_leaf(&block, &free);
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
