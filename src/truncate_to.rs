//! Truncating a file to any length, through the log (#370).
//!
//! [`Filesystem::truncate_to_zero`] frees a file's every block, and
//! [`Filesystem::truncate`] lowers its size in place and keeps the blocks
//! past the new end. This is the truncate the kernel does: to any length,
//! shorter or longer, freeing what falls past the new end, in one record.
//!
//! # Shrinking
//!
//! Each extent that ends at or before the new last block is kept as it
//! is. One that begins after it is freed whole. One that straddles it is
//! cut: the blocks still inside the file stay, and the rest are freed. Its
//! reverse-mapping record is replaced by one for the part that stays, and
//! the reference-count tree decides which freed blocks go back to free
//! space, so blocks another file still shares never do.
//!
//! The last block, when the new size ends inside it, keeps bytes past the
//! new end that the file must not show again: they are zeroed on disk
//! before the record, as the kernel zeroes them before it lowers the size.
//! A block shared with another file is not zeroed in place, which would
//! change what that file reads, and that truncate is refused.
//!
//! An extent B+tree whose remaining extents fit in the inode becomes an
//! inline extent list again, and the tree's blocks are freed with the
//! data. One that still needs a tree is refused by name.
//!
//! # Growing
//!
//! A larger size reads as a hole past the old end. The old last block's
//! bytes past the old end become visible, so they are zeroed first.

use crate::create::clock_now;
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::format::log_items::inode_log_format::XFS_ILOG_DEXT;
use crate::fs::Filesystem;
use crate::group_write::{split_fsblock, Allocations};
use crate::inode::{stamp_change, Changed, Format};
use crate::log_write::{
    inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer,
    Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that a truncate changes.
mod at {
    pub const FORMAT: usize = 5;
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    pub const NEXTENTS: usize = 76;
    pub const NEXTENTS64: usize = 24;
    pub const CHANGECOUNT: usize = 104;
    pub const FLAGS2: usize = 120;
}

/// An extent's reverse-mapping offset: its file offset, with the unwritten
/// flag the kernel keeps beside it.
fn rmap_offset(e: &Extent) -> u64 {
    e.startoff
        | if e.unwritten {
            crate::rmap::OFF_UNWRITTEN
        } else {
            0
        }
}

impl Filesystem {
    /// Truncate the regular file `ino` to `new_size` bytes, through the
    /// log, freeing every block past the new end.
    ///
    /// Returns the sequence number the record was given, or 0 when the
    /// size is already `new_size` and nothing changes.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for an inline or real-time file, a
    /// partial last block another file shares, and an extent B+tree that
    /// still needs a tree. Every refusal comes before anything is written.
    pub fn truncate_to(&self, ino: u64, new_size: u64) -> Result<u64> {
        let device = self.writable_device()?;
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
                "inode {ino} keeps its data on the real-time device"
            )));
        }
        if file.format == Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} stores its data inside the inode"
            )));
        }
        if new_size == file.size {
            return Ok(0);
        }
        let bs = u64::from(self.sb.blocksize);
        let (extents, map_blocks) = match file.format {
            Format::Btree => {
                let (start, end) = file.data_fork_range(usize::from(self.sb.inodesize));
                crate::bmbt::walk_with_blocks(
                    &raw[start..end],
                    file.nextents,
                    &self.sb,
                    ino,
                    |b| self.read_fsblock(b),
                )?
            }
            _ => (self.data_extents(&file, &raw)?, Vec::new()),
        };
        let written_at = |fb: u64| -> Option<u64> {
            extents
                .iter()
                .find(|e| !e.unwritten && fb >= e.startoff && fb < e.startoff + e.blockcount)
                .map(|e| e.startblock + (fb - e.startoff))
        };

        // The bytes past the old or new end that the file would show again:
        // the tail of the block the smaller of the two sizes ends in.
        let edge = new_size.min(file.size);
        let zero_tail = if edge % bs != 0 {
            written_at(edge / bs).map(|phys| (phys, edge % bs))
        } else {
            None
        };
        if zero_tail.is_some() && file.has_shared_extents() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} has reflinked extents, and zeroing the part of its last block \
                 past {edge} in place could change what another file reads"
            )));
        }

        // What stays, and what goes.
        let keep_blocks = new_size.div_ceil(bs);
        let mut kept: Vec<Extent> = Vec::new();
        let mut freeing: Vec<(Extent, Option<Extent>)> = Vec::new();
        for e in &extents {
            let end = e.startoff + e.blockcount;
            if new_size >= file.size || end <= keep_blocks {
                kept.push(*e);
            } else if e.startoff >= keep_blocks {
                freeing.push((*e, None));
            } else {
                let stay = Extent {
                    blockcount: keep_blocks - e.startoff,
                    ..*e
                };
                kept.push(stay);
                freeing.push((*e, Some(stay)));
            }
        }
        // A truncate that frees nothing, a grow or a cut inside the last
        // block, leaves the extent map as it is, B+tree or not, and logs the
        // core alone.
        let map_changes = !freeing.is_empty();
        let (fork_start, fork_end) = file.data_fork_range(usize::from(self.sb.inodesize));
        let room = (fork_end - fork_start) / 16;
        if map_changes && kept.len() > room {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps {} extents, more than its fork lists, so its extent \
                 B+tree would have to be rewritten rather than freed",
                kept.len()
            )));
        }
        let drop_tree = map_changes && file.format == Format::Btree;

        let mut allocations = Allocations::new();
        let mut freed = 0u64;
        for (whole, stay) in &freeing {
            let (agno, agblock) = split_fsblock(&self.sb, whole.startblock);
            let group = allocations.group(&self.sb, self.device(), agno)?;
            let count = |n: u64| -> Result<u32> {
                u32::try_from(n).map_err(|_| {
                    Error::UnsupportedFeature(format!("inode {ino} has an extent of {n} blocks"))
                })
            };
            group.forget_rmap(crate::rmap::Rmap {
                startblock: agblock,
                blockcount: count(whole.blockcount)?,
                owner: ino as i64,
                offset: rmap_offset(whole),
            })?;
            let kept_blocks = stay.map_or(0, |s| s.blockcount);
            if let Some(s) = stay {
                group.remember_rmap(crate::rmap::Rmap {
                    startblock: agblock,
                    blockcount: count(s.blockcount)?,
                    owner: ino as i64,
                    offset: rmap_offset(s),
                })?;
            }
            let free_start = agblock + count(kept_blocks)?;
            let free_len = count(whole.blockcount - kept_blocks)?;
            for range in group.release_shared(free_start, free_len)? {
                group.give_back(range)?;
            }
            freed += whole.blockcount - kept_blocks;
        }
        if drop_tree {
            // The map's own blocks, in the runs the reverse map holds them.
            let mut by_group: std::collections::BTreeMap<u32, Vec<u32>> = Default::default();
            for b in &map_blocks {
                let (agno, agblock) = split_fsblock(&self.sb, *b);
                by_group.entry(agno).or_default().push(agblock);
            }
            for (agno, mut blocks) in by_group {
                blocks.sort_unstable();
                blocks.dedup();
                let group = allocations.group(&self.sb, self.device(), agno)?;
                let mut i = 0;
                while i < blocks.len() {
                    let start = blocks[i];
                    let mut len = 1u32;
                    while i + 1 < blocks.len() && blocks[i + 1] == start + len {
                        len += 1;
                        i += 1;
                    }
                    i += 1;
                    group.forget_rmap(crate::rmap::Rmap {
                        startblock: start,
                        blockcount: len,
                        owner: ino as i64,
                        offset: crate::rmap::OFF_BMBT_BLOCK,
                    })?;
                    for range in group.release_shared(start, len)? {
                        group.give_back(range)?;
                    }
                    freed += u64::from(len);
                }
            }
        }
        let allocation_items = allocations.into_items()?;
        let quota_items = if freed == 0 {
            Vec::new()
        } else {
            crate::quota::accounting_items(
                self,
                &[crate::quota::QuotaChange {
                    uid: file.uid,
                    gid: file.gid,
                    project_id: crate::quota::project_id(&raw),
                    blocks_fs: -(freed as i64),
                    inodes: 0,
                }],
            )?
        };

        let mut fork = Vec::with_capacity(kept.len() * 16);
        for e in &kept {
            fork.extend_from_slice(&e.to_bytes()?);
        }
        let mut core = raw.clone();
        if drop_tree {
            core[at::FORMAT] = Format::Extents as u8;
        }
        core[at::SIZE..at::SIZE + 8].copy_from_slice(&new_size.to_be_bytes());
        core[at::NBLOCKS..at::NBLOCKS + 8].copy_from_slice(&(file.nblocks - freed).to_be_bytes());
        let nrext64 = u64::from_be_bytes(raw[at::FLAGS2..at::FLAGS2 + 8].try_into().expect("8"))
            & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
            != 0;
        if !map_changes {
            // The extent count stays what it was.
        } else if nrext64 {
            core[at::NEXTENTS64..at::NEXTENTS64 + 8]
                .copy_from_slice(&(kept.len() as u64).to_be_bytes());
        } else {
            core[at::NEXTENTS..at::NEXTENTS + 4]
                .copy_from_slice(&(kept.len() as u32).to_be_bytes());
        }
        let now = u64::from_be_bytes(
            core[at::CHANGECOUNT..at::CHANGECOUNT + 8]
                .try_into()
                .expect("8"),
        );
        core[at::CHANGECOUNT..at::CHANGECOUNT + 8]
            .copy_from_slice(&now.wrapping_add(1).to_be_bytes());
        stamp_change(&mut core, clock_now(), Changed::Contents);
        let logged = log_dinode_from_disk(&core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());

        // Every refusal is behind us. The tail goes down first, the record
        // last, as the kernel zeroes before it changes the size.
        if let Some((phys, from)) = zero_tail {
            device.write_at(
                self.sb.fsblock_offset(phys) + from,
                &vec![0u8; (bs - from) as usize],
            )?;
            device.flush()?;
        }

        let dsize = fork.len();
        let mut fork_op = fork.clone();
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);
        let item_ops = allocation_items.iter().map(|i| i.op_count()).sum::<usize>()
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + if map_changes { 3 } else { 2 };
        let lsn = self.commit_record(|tid| {
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
            for item in &allocation_items {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            if map_changes {
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
            } else {
                ops.push(Op {
                    flags: 0,
                    data: inode_log_format(ino, XFS_ILOG_CORE, &buffer),
                });
                ops.push(Op {
                    flags: 0,
                    data: logged,
                });
            }
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&allocation_items);
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        if map_changes {
            self.logged_inode(ino, &core, &fork)?;
        } else {
            let (start, end) = file.data_fork_range(usize::from(self.sb.inodesize));
            self.logged_inode(ino, &core, &raw[start..end])?;
        }
        Ok(lsn)
    }
}
