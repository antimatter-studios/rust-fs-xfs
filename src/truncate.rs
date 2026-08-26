//! Truncating a file to nothing, through the log.
//!
//! This is the first transaction that changes something other than an
//! inode. Freeing a file's blocks means putting them back into the
//! allocation group's two free-space B+trees and correcting the totals
//! in the group header, so the record carries three buffer items beside
//! the inode — and buffer items are what every remaining operation
//! (allocation, create, unlink, any directory past shortform) is built
//! from.
//!
//! ```text
//! op  what
//!  0   START
//!  1   transaction header
//!  2   the group header's buffer item, and its dirty chunk
//!  4   the by-block tree's root, and its dirty chunks
//!  ..  the by-length tree's root, and its dirty chunks
//!  ..  the inode's format and core
//!  ..  COMMIT
//! ```
//!
//! Eleven operations for the smallest case, which is what a truncate was
//! measured to produce. The count is computed from the items rather than
//! fixed, because how many chunks of a tree block change depends on
//! where in it the record went.
//!
//! # Nothing on disk is touched
//!
//! As with the inode-core and rename cases, only the record is written.
//! The record is the durable statement of the change and whatever mounts
//! the filesystem next applies it — which is also what makes the result
//! checkable, since a free-space tree that changed is a tree something
//! replayed.
//!
//! # What it will not do
//!
//! Each is refused by name rather than attempted:
//!
//! - a file whose extents live in more than one allocation group;
//! - a data fork in B+tree format, whose own tree blocks would have to
//!   be freed as well as the file's data;
//! - an allocation group whose free-space trees are more than one level
//!   deep, where inserting a record can split a node;
//! - a tree root with no room for another record, for the same reason;
//! - a real-time file, whose blocks are not in an allocation group at
//!   all.

use crate::ag::Agf;
use crate::alloc_btree::{expected_blkno, free_extent, longest, total_free, FreeExtent};
use crate::error::{Error, Result};
use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGF, BLFT_BTREE};
use crate::fs::Filesystem;
use crate::group_write::{
    agf, btree, changed_chunks, emptied_core, leaf_capacity, leaf_records, rebuild_leaf,
    split_fsblock,
};
use crate::log::BBSIZE;
use crate::log_write::{
    append, inode_log_format, log_dinode_from_disk, trans_header, InodeBuffer, Op, XFS_ILOG_CORE,
    XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};
impl Filesystem {
    /// Truncate `ino` to nothing, writing the change to the log.
    ///
    /// The file's blocks go back to their allocation group's free-space
    /// trees and the inode is emptied. Nothing on disk is touched: the
    /// record is the change, and whatever mounts the filesystem next
    /// applies it.
    ///
    /// Returns the sequence number the record was given.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for each of the shapes listed in
    /// this module's documentation.
    pub fn truncate_to_zero(&self, ino: u64) -> Result<u64> {
        self.begin_checkpoint()?;
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "truncating writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }

        let (file, raw) = self.read_inode_raw(ino)?;
        if !file.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if file.is_realtime() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps its data on the real-time device, which has no \
                 allocation groups to free into"
            )));
        }
        if file.format == crate::inode::Format::Btree {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps its extents in a B+tree, whose own blocks would have \
                 to be freed alongside the file's; only an inline extent list is supported"
            )));
        }

        let extents = self.data_extents(&file, &raw)?;
        if extents.is_empty() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} has no extents to free"
            )));
        }

        // One group only. Freeing into several means one buffer item per
        // tree per group, and a checkpoint that no longer resembles the
        // shape this was measured against.
        let (agno, _) = split_fsblock(&self.sb, extents[0].startblock);
        let mut freeing = Vec::with_capacity(extents.len());
        for extent in &extents {
            let (owner, agblock) = split_fsblock(&self.sb, extent.startblock);
            if owner != agno {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} has extents in allocation groups {agno} and {owner}; \
                     freeing across groups is not implemented"
                )));
            }
            freeing.push(FreeExtent {
                startblock: agblock,
                blockcount: u32::try_from(extent.blockcount).map_err(|_| {
                    Error::UnsupportedFeature(format!(
                        "inode {ino} has an extent of {} blocks, more than a group can hold",
                        extent.blockcount
                    ))
                })?,
            });
        }

        let block = u64::from(self.sb.blocksize);
        let ag_start = u64::from(agno) * u64::from(self.sb.agblocks) * block;
        let sector = u64::from(self.sb.sectsize);

        let mut agf_raw = vec![0u8; self.sb.sectsize as usize];
        self.device().read_at(ag_start + sector, &mut agf_raw)?;
        let agf = Agf::parse(&agf_raw, &self.sb, agno)?;

        use crate::ag::agf_btree::{BNO, CNT};
        for (which, name) in [(BNO, "by-block"), (CNT, "by-length")] {
            if agf.levels[which] != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno}'s {name} free-space tree is {} levels deep, \
                     where inserting a record can split a node; only a single-level tree \
                     is supported",
                    agf.levels[which]
                )));
            }
        }

        let mut bno_raw = vec![0u8; self.sb.blocksize as usize];
        self.device()
            .read_at(ag_start + u64::from(agf.roots[BNO]) * block, &mut bno_raw)?;
        let mut cnt_raw = vec![0u8; self.sb.blocksize as usize];
        self.device()
            .read_at(ag_start + u64::from(agf.roots[CNT]) * block, &mut cnt_raw)?;

        let numrecs = u16::from_be_bytes(
            bno_raw[btree::NUMRECS..btree::NUMRECS + 2]
                .try_into()
                .expect("2 bytes"),
        );
        let mut by_block = leaf_records(&bno_raw, numrecs);

        for extent in &freeing {
            free_extent(&mut by_block, *extent)?;
        }

        let capacity = leaf_capacity(self.sb.blocksize);
        if by_block.len() > capacity {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno} would need {} free-space records and its tree root \
                 holds {capacity}; splitting a node is not implemented",
                by_block.len()
            )));
        }

        // The second tree holds the same extents ordered by length, and
        // equal lengths are ordered by start block so the ordering is
        // total.
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
        // The checksum is left stale on purpose — recovery recomputes it,
        // and writing it here would dirty a chunk nothing else touches and
        // add an operation to the record. See `group_write::restamp_crc`.

        // Addresses are in 512-byte basic blocks, absolute on the
        // device. The group header is its second sector.
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

        let core = emptied_core(&raw, true);
        let logged = log_dinode_from_disk(&core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());

        // The header counts operations belonging to items, so it is the
        // items' own counts summed rather than a constant — how many
        // chunks of a tree block changed depends on where the record
        // went.
        let item_ops = agf_item.op_count() + bno_item.op_count() + cnt_item.op_count() + 2;

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
                data: inode_log_format(ino, XFS_ILOG_CORE, &buffer),
            });
            ops.push(Op {
                flags: 0,
                data: logged,
            });
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })
    }
}
