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

use crate::alloc_btree::FreeExtent;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::group_write::{emptied_core, split_fsblock};
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
    ///
    /// # This is not a special case of [`Filesystem::truncate`]
    ///
    /// The names suggest it is. It is the **more** complete of the two:
    ///
    /// | | `truncate_to_zero` | `truncate` |
    /// |---|---|---|
    /// | journalled | **yes** — writes a log record and nothing else | **no** — writes straight to the device |
    /// | the blocks | **freed** into the group's free-space trees | **kept**, past end of file |
    /// | takes | an inode number | an `&Inode` you already read |
    /// | new size | zero | any size not larger |
    ///
    /// Freeing the blocks is the allocation work `truncate` avoids
    /// precisely so it can skip the log. That is the whole difference,
    /// and it is the one a caller most needs to know: after `truncate`,
    /// `du` still reports the old size while `ls` does not.
    ///
    /// Neither name says any of that, and renaming a published function
    /// is a decision rather than a correction — so until one is made,
    /// each says it here.
    /// Give one allocation group's worth of blocks back, and say what
    /// changed.
    ///
    /// Everything a free touches is per-group — the header, the two
    /// free-space trees, the reverse map and the reference-count tree —
    /// so a file whose extents are in several groups is this, several
    /// times, and nothing about it is singular.
    ///
    /// Returns the buffer items. Nothing is written: the items are the
    /// change, and the caller puts them in a record.
    fn free_in_group(
        &self,
        ino: u64,
        agno: u32,
        freeing_here: &[FreeExtent],
        extents_here: &[crate::extent::Extent],
    ) -> Result<Vec<crate::buf_write::BufferItem>> {
        // ONE EDITOR FOR THE GROUP, at whatever depth its trees are.
        //
        // This read the four trees itself, refused any of them deeper
        // than one block, and wrote each one back by rewriting its root
        // -- which is the shape a fresh filesystem has and no filesystem
        // in use keeps. `GroupAlloc` reads them with the walker and lays
        // them out again, so a group whose free space is fragmented is
        // one this can free into.
        let mut group = crate::group_write::GroupAlloc::open(&self.sb, self.device(), agno)?;

        // THE REVERSE MAP FIRST, and matched exactly. This frees a
        // file's map entire, so a record that does not line up means the
        // tree and the inode disagree, and the free must not go ahead on
        // top of that.
        for extent in extents_here {
            let (_, agblock) = crate::group_write::split_fsblock(&self.sb, extent.startblock);
            group.forget_rmap(crate::rmap::Rmap {
                startblock: agblock,
                blockcount: extent.blockcount as u32,
                owner: ino as i64,
                offset: extent.startoff,
            })?;
        }

        // WHAT MAY ACTUALLY GO BACK TO FREE SPACE.
        //
        // On a reflink filesystem an extent can have more than one
        // owner, and returning its blocks while another file still
        // points at them is the worst thing available here: the
        // allocator hands them out again and the two files overwrite
        // each other. The reference-count tree decides, per range,
        // because one extent can be part shared and part not -- and
        // blocks another file still holds are never returned.
        for extent in freeing_here {
            for range in group.release_shared(extent.startblock, extent.blockcount)? {
                group.give_back(range)?;
            }
        }

        let items = group.into_items()?;
        Ok(items)
    }

    pub fn truncate_to_zero(&self, ino: u64) -> Result<u64> {
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

        // ONE GROUP AT A TIME, however many there are.
        //
        // A file bigger than an allocation group has to be split across
        // several, and then everything a free touches — the header, both
        // free-space trees, the reverse map, the reference-count tree —
        // exists once per group. This refused that case, which meant a
        // file larger than a group could never be truncated, and a group
        // is 75 MB on the fixture that found it.
        //
        // The groups are visited in order so the record's items come out
        // in a fixed sequence rather than in whatever order the file's
        // extents happen to be in.
        let mut by_group: std::collections::BTreeMap<
            u32,
            (Vec<FreeExtent>, Vec<crate::extent::Extent>),
        > = std::collections::BTreeMap::new();
        for extent in &extents {
            let (owner, agblock) = split_fsblock(&self.sb, extent.startblock);
            let entry = by_group.entry(owner).or_default();
            entry.0.push(FreeExtent {
                startblock: agblock,
                blockcount: u32::try_from(extent.blockcount).map_err(|_| {
                    Error::UnsupportedFeature(format!(
                        "inode {ino} has an extent of {} blocks, more than a group can hold",
                        extent.blockcount
                    ))
                })?,
            });
            entry.1.push(*extent);
        }

        let mut group_items = Vec::new();
        for (agno, (freeing_here, extents_here)) in &by_group {
            group_items.extend(self.free_in_group(ino, *agno, freeing_here, extents_here)?);
        }

        let core = emptied_core(&raw, true);
        let logged = log_dinode_from_disk(&core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());

        // The header counts operations belonging to items, so it is the
        // items' own counts summed rather than a constant — how many
        // chunks of a tree block changed depends on where the record
        // went.
        let item_ops = group_items.iter().map(|i| i.op_count()).sum::<usize>() + 2;

        // Every refusal this operation has is behind us and the next
        // statement writes, so the mount's one checkpoint is claimed
        // here rather than on the way in: a refusal must not spend it.
        // See `Filesystem::begin_checkpoint`.
        self.begin_checkpoint()?;
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
            for item in &group_items {
                ops.extend(item.ops());
            }
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
