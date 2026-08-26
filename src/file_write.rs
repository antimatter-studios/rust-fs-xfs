//! Giving a file blocks it did not have, through the log.
//!
//! [`crate::write`] can overwrite bytes that already exist, because an
//! overwrite touches no metadata and so needs no journal. Everything
//! else a write might do — making a file longer, giving an empty one
//! contents at all — has to take blocks out of an allocation group and
//! say so in a record. This is that.
//!
//! It is the mirror of [`crate::truncate`], and shares its machinery
//! through [`crate::group_write`]: the same group header, the same two
//! free-space trees, the same buffer items. The difference is the
//! direction the records move, and one thing that has no counterpart in
//! truncating.
//!
//! # The file's contents are not in the record
//!
//! XFS journals metadata, not data. So this writes the file's bytes
//! **straight to their blocks** and logs only the change to the free
//! space and the inode. That is not a shortcut — it is what XFS does,
//! and it is why a torn write leaves a file holding a mixture of old and
//! new bytes rather than a filesystem that has to be repaired.
//!
//! It does mean the ordering matters. The data goes down first and the
//! record last, so a machine that dies between them leaves blocks that
//! were written but never claimed: lost space until the next repair,
//! rather than a file pointing at blocks holding something else.
//!
//! ```text
//! op  what
//!  0   START
//!  1   transaction header
//!  2   the group header's buffer item, and its dirty chunk
//!  ..  the by-block tree's root, and its dirty chunks
//!  ..  the by-length tree's root, and its dirty chunks
//!  ..  the inode's format, core and extent list
//!  ..  COMMIT
//! ```
//!
//! Twelve operations for the smallest case — one more than a truncate,
//! because the inode logs its extent list as well as its core.
//!
//! # Which blocks it takes
//!
//! The first free run long enough to hold the file, in the allocation
//! group the inode itself lives in.
//!
//! That policy is this driver's, not XFS's — XFS weighs locality,
//! contiguity and several other things, and none of that is visible in a
//! record. Any run that is genuinely free produces a filesystem the
//! kernel accepts; the choice only affects how well laid out it is.
//! Saying so plainly is better than implying a fidelity that is not
//! there.
//!
//! # What it will not do
//!
//! Each is refused by name rather than attempted:
//!
//! - a file that already has extents, since appending to one means
//!   merging with or following the extents it has;
//! - a file too large for one extent, or for the room the inode has to
//!   record one;
//! - a group whose free-space trees are more than one level deep, or
//!   whose root has no room for another record;
//! - no single free run long enough, which needs more than one extent;
//! - a real-time file and a v4 filesystem.

use crate::ag::Agf;
use crate::alloc_btree::{alloc_extent, expected_blkno, longest, total_free, FreeExtent};
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGF, BLFT_BTREE};
use crate::fs::Filesystem;
use crate::group_write::{
    agf, btree, changed_chunks, leaf_capacity, leaf_records, rebuild_leaf, restamp_crc,
};
use crate::inode::Format;
use crate::log::BBSIZE;
use crate::log_write::{
    append, inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer, Op,
    XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

use crate::format::log_items::inode_log_format::XFS_ILOG_DEXT;

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that gaining blocks changes.
mod inode {
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    pub const NEXTENTS: usize = 76;
    pub const NEXTENTS64: usize = 24;
    pub const CHANGECOUNT: usize = 104;
    pub const FLAGS2: usize = 120;
}

/// The inode core of a file that now holds `blocks` blocks and `size`
/// bytes in one extent.
fn filled_core(raw: &[u8], size: u64, blocks: u64) -> Vec<u8> {
    let mut core = raw.to_vec();
    core[inode::SIZE..inode::SIZE + 8].copy_from_slice(&size.to_be_bytes());
    core[inode::NBLOCKS..inode::NBLOCKS + 8].copy_from_slice(&blocks.to_be_bytes());

    // Where the data-extent count lives depends on a feature bit in the
    // inode itself rather than in the superblock, because it is the
    // inode's own encoding that matters.
    let nrext64 = u64::from_be_bytes(
        raw[inode::FLAGS2..inode::FLAGS2 + 8]
            .try_into()
            .expect("8 bytes"),
    ) & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
        != 0;
    if nrext64 {
        core[inode::NEXTENTS64..inode::NEXTENTS64 + 8].copy_from_slice(&1u64.to_be_bytes());
    } else {
        core[inode::NEXTENTS..inode::NEXTENTS + 4].copy_from_slice(&1u32.to_be_bytes());
    }

    let at = inode::CHANGECOUNT;
    let now = u64::from_be_bytes(core[at..at + 8].try_into().expect("8 bytes"));
    core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
    core
}

impl Filesystem {
    /// Give an empty file `data` as its contents, writing the blocks and
    /// logging the change.
    ///
    /// The bytes go to their blocks directly and only the metadata is
    /// logged, which is what XFS does. Returns the sequence number the
    /// record was given.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for each of the shapes listed in
    /// this module's documentation.
    pub fn write_into_empty_file(&self, ino: u64, data: &[u8]) -> Result<u64> {
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "writing allocates v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if data.is_empty() {
            return Err(Error::UnsupportedFeature(
                "a write of no bytes allocates nothing and has nothing to log".into(),
            ));
        }

        let (file, raw) = self.read_inode_raw(ino)?;
        if !file.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if file.is_realtime() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps its data on the real-time device, which has no \
                 allocation groups to take blocks from"
            )));
        }
        if file.format != Format::Extents || file.nextents != 0 || file.size != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} already holds {} bytes in {} extents; appending means merging \
                 with or following the extents it has, which is not implemented",
                file.size, file.nextents
            )));
        }

        let blocksize = u64::from(self.sb.blocksize);
        let blocks = (data.len() as u64).div_ceil(blocksize);
        let want = u32::try_from(blocks).map_err(|_| {
            Error::UnsupportedFeature(format!(
                "{} bytes needs {blocks} blocks, more than one extent can hold",
                data.len()
            ))
        })?;

        // The inode's own group, for locality. Any group would produce a
        // filesystem the kernel accepts; this one keeps the file near
        // the inode that names it.
        let (agno, _, _) = self.sb.split_ino(ino);
        if agno >= self.sb.agcount {
            return Err(Error::BadSuperblock(format!(
                "inode {ino} names allocation group {agno}, but there are only {}",
                self.sb.agcount
            )));
        }
        let ag_start = u64::from(agno) * u64::from(self.sb.agblocks) * blocksize;
        let sector = u64::from(self.sb.sectsize);

        let mut agf_raw = vec![0u8; self.sb.sectsize as usize];
        self.device().read_at(ag_start + sector, &mut agf_raw)?;
        let agf = Agf::parse(&agf_raw, &self.sb, agno)?;

        use crate::ag::agf_btree::{BNO, CNT};
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

        // First fit, in block order. See the note on policy at the top:
        // the choice is this driver's and affects layout, not
        // correctness.
        let chosen = by_block
            .iter()
            .find(|run| run.blockcount >= want)
            .copied()
            .ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "allocation group {agno} has no single free run of {want} blocks — its \
                     longest is {}, and splitting a file across extents is not implemented",
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
        restamp_crc(&mut new_agf, agf::CRC);

        // The file's own bytes, straight to their blocks, before the
        // record that claims them. A machine that dies between the two
        // leaves blocks written but unclaimed — lost space, not a file
        // pointing at someone else's data.
        let at = ag_start + u64::from(taking.startblock) * blocksize;
        let mut padded = data.to_vec();
        padded.resize((blocks * blocksize) as usize, 0);
        device.write_at(at, &padded)?;
        device.flush()?;

        // The extent record names a filesystem block, which packs the
        // group and the block within it.
        let fsblock = (u64::from(agno) << self.sb.agblklog) | u64::from(taking.startblock);
        let extent = Extent {
            startoff: 0,
            startblock: fsblock,
            blockcount: blocks,
            unwritten: false,
        };
        let fork = extent.to_bytes()?.to_vec();
        let dsize = fork.len();
        let mut fork_op = fork;
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);

        let ag_bb = ag_start / BBSIZE as u64;
        let agf_item = changed_chunks(ag_bb + sector / BBSIZE as u64, &agf_raw, new_agf, BLFT_AGF);
        let bno_item = changed_chunks(
            expected_blkno(&self.sb, agno, agf.roots[BNO]),
            &bno_raw,
            new_bno,
            BLFT_BTREE,
        );
        let cnt_item = changed_chunks(
            expected_blkno(&self.sb, agno, agf.roots[CNT]),
            &cnt_raw,
            new_cnt,
            BLFT_BTREE,
        );

        let core = filled_core(&raw, data.len() as u64, blocks);
        let logged = log_dinode_from_disk(&core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());

        // Three operations for the inode this time — format, core and
        // extent list — where a truncate logs two.
        let item_ops = agf_item.op_count() + bno_item.op_count() + cnt_item.op_count() + 3;

        append(device.as_ref(), &self.sb, |tid| {
            let mut ops = vec![
                Op {
                    flags: XLOG_START_TRANS,
                    data: Vec::new(),
                },
                Op {
                    flags: 0,
                    data: trans_header(tid, XFS_TRANS_CHECKPOINT, item_ops as u32),
                },
            ];
            ops.extend(agf_item.ops());
            ops.extend(bno_item.ops());
            ops.extend(cnt_item.ops());
            ops.push(Op {
                flags: 0,
                data: inode_log_format_with_fork(
                    ino,
                    XFS_ILOG_CORE | XFS_ILOG_DEXT,
                    &buffer,
                    dsize as u16,
                ),
            });
            ops.push(Op {
                flags: 0,
                data: logged,
            });
            ops.push(Op {
                flags: 0,
                data: fork_op,
            });
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that has just been given blocks says so in three places,
    /// and each of them is read by something different: the size by
    /// anything reading the file, the block count by quota and repair,
    /// the extent count by the fork parser.
    #[test]
    fn a_filled_core_records_the_size_the_blocks_and_the_extent() {
        let mut raw = vec![0u8; 176];
        // A v3 inode with neither bigtime nor nrext64.
        raw[inode::FLAGS2..inode::FLAGS2 + 8].copy_from_slice(&0u64.to_be_bytes());

        let core = filled_core(&raw, 1000, 1);
        assert_eq!(
            u64::from_be_bytes(core[inode::SIZE..inode::SIZE + 8].try_into().unwrap()),
            1000
        );
        assert_eq!(
            u64::from_be_bytes(core[inode::NBLOCKS..inode::NBLOCKS + 8].try_into().unwrap()),
            1
        );
        assert_eq!(
            u32::from_be_bytes(
                core[inode::NEXTENTS..inode::NEXTENTS + 4]
                    .try_into()
                    .unwrap()
            ),
            1
        );
    }

    /// Under `nrext64` the data-extent count is a 64-bit field somewhere
    /// else entirely, and writing the old one would leave the inode
    /// claiming no extents while holding one.
    #[test]
    fn the_extent_count_follows_the_inodes_own_feature_bit() {
        use crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64;

        let mut raw = vec![0u8; 176];
        raw[inode::FLAGS2..inode::FLAGS2 + 8].copy_from_slice(&DI_FLAGS2_NREXT64.to_be_bytes());

        let core = filled_core(&raw, 1000, 1);
        assert_eq!(
            u64::from_be_bytes(
                core[inode::NEXTENTS64..inode::NEXTENTS64 + 8]
                    .try_into()
                    .unwrap()
            ),
            1
        );
        assert_eq!(
            u32::from_be_bytes(
                core[inode::NEXTENTS..inode::NEXTENTS + 4]
                    .try_into()
                    .unwrap()
            ),
            0,
            "the 32-bit count must be left alone under nrext64"
        );
    }

    /// The change counter is what says the inode has moved on.
    #[test]
    fn the_change_counter_advances() {
        let mut raw = vec![0u8; 176];
        raw[inode::CHANGECOUNT..inode::CHANGECOUNT + 8].copy_from_slice(&41u64.to_be_bytes());
        let core = filled_core(&raw, 1, 1);
        assert_eq!(
            u64::from_be_bytes(
                core[inode::CHANGECOUNT..inode::CHANGECOUNT + 8]
                    .try_into()
                    .unwrap()
            ),
            42
        );
    }
}
