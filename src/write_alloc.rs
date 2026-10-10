//! A write anywhere in a file, through the log (#387).
//!
//! [`crate::write`] overwrites bytes that already exist and refuses the
//! rest, and [`crate::file_write`] gives an empty file its first extent.
//! This is the write between them: any offset, any length, in a file that
//! may already have extents, holes and unwritten ranges.
//!
//! Each block the write covers is one of three things, and the write
//! treats it as that:
//!
//! - **written**: the bytes are overwritten in place, and nothing else in
//!   the block moves;
//! - **a hole**: blocks are taken from the inode's allocation group and a
//!   new written extent maps them;
//! - **unwritten**: the part of the extent the write covers becomes a
//!   written extent of its own, and what is left on either side stays
//!   unwritten.
//!
//! A block the write takes or converts is written whole: the caller's
//! bytes where they fall, and **zeros everywhere else**, because a hole
//! and an unwritten extent both read as zeros and the block is about to
//! read as whatever is on disk. A write past the end of the file also
//! zeroes the old last block's tail past the old end, which a reader
//! could not see before and can now.
//!
//! As in [`crate::file_write`], the data goes to its blocks first and the
//! record last, so a crash between them leaves written blocks no extent
//! claims, rather than an extent claiming blocks that hold something
//! else. The record carries the group's trees, quota, and the inode's core
//! and whole extent list.
//!
//! # What it will not do
//!
//! Each is refused by name before anything is written:
//!
//! - a file whose extents are in a B+tree, or a write that would leave
//!   more extents than the inode has room to list;
//! - a hole longer than one free run in the inode's group;
//! - a reflinked, real-time or inline file, and a v4 filesystem.

use crate::create::clock_now;
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::fs::Filesystem;
use crate::inode::{stamp_change, Changed, Format};
use crate::log_write::{
    inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer, Op, XFS_ILOG_CORE,
    XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

use crate::format::log_items::inode_log_format::XFS_ILOG_DEXT;

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// The size of one extent record in an inode's fork.
const EXTENT_BYTES: usize = 16;

/// The most blocks one extent record can map: a 21-bit field.
const MAX_EXTENT_BLOCKS: u64 = (1 << 21) - 1;

/// The highest file block an extent can start at: a 54-bit field.
const MAX_FILE_BLOCK: u64 = (1 << 54) - 1;

/// Offsets within the on-disk inode core that a write changes.
mod at {
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    pub const NEXTENTS: usize = 76;
    pub const NEXTENTS64: usize = 24;
    pub const CHANGECOUNT: usize = 104;
    pub const FLAGS2: usize = 120;
}

/// The inode core of a file now `size` bytes long, holding `blocks`
/// blocks in `extents` extents.
fn grown_core(raw: &[u8], size: u64, blocks: u64, extents: u64) -> Result<Vec<u8>> {
    let mut core = raw.to_vec();
    core[at::SIZE..at::SIZE + 8].copy_from_slice(&size.to_be_bytes());
    core[at::NBLOCKS..at::NBLOCKS + 8].copy_from_slice(&blocks.to_be_bytes());
    let nrext64 = u64::from_be_bytes(raw[at::FLAGS2..at::FLAGS2 + 8].try_into().expect("8 bytes"))
        & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
        != 0;
    if nrext64 {
        core[at::NEXTENTS64..at::NEXTENTS64 + 8].copy_from_slice(&extents.to_be_bytes());
    } else {
        let count = u32::try_from(extents).map_err(|_| {
            Error::UnsupportedFeature(format!("{extents} extents overflow the 32-bit count"))
        })?;
        core[at::NEXTENTS..at::NEXTENTS + 4].copy_from_slice(&count.to_be_bytes());
    }
    let now = u64::from_be_bytes(
        core[at::CHANGECOUNT..at::CHANGECOUNT + 8]
            .try_into()
            .expect("8 bytes"),
    );
    core[at::CHANGECOUNT..at::CHANGECOUNT + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
    stamp_change(&mut core, clock_now(), Changed::Contents);
    Ok(core)
}

/// What a write does to one block of the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    /// Written already: overwrite the bytes the write covers, in place.
    InPlace(u64),
    /// Newly taken, or converted from unwritten: write the whole block,
    /// zeros where the write does not reach.
    Whole(u64),
}

/// The new extent list, and how each block of `first..=last` is written.
struct Plan {
    extents: Vec<Extent>,
    /// The holes in `first..=last`, as `(file block, blocks)`, still to be
    /// given blocks.
    holes: Vec<(u64, u64)>,
    /// Block number in the file → the device block it is written to, for
    /// every block already mapped.
    mapped: Vec<(u64, Block)>,
}

/// Split `extents` against the file blocks `first..=last`.
fn plan(extents: &[Extent], first: u64, last: u64) -> Plan {
    let mut out = Plan {
        extents: Vec::new(),
        holes: Vec::new(),
        mapped: Vec::new(),
    };
    let mut cursor = first;
    let mut sorted = extents.to_vec();
    sorted.sort_by_key(|e| e.startoff);
    for e in sorted {
        let end = e.startoff + e.blockcount;
        if end <= first || e.startoff > last {
            out.extents.push(e);
            continue;
        }
        if e.startoff > cursor {
            out.holes.push((cursor, e.startoff - cursor));
        }
        let lo = e.startoff.max(first);
        let hi = end.min(last + 1);
        if e.unwritten {
            if e.startoff < lo {
                out.extents.push(Extent {
                    blockcount: lo - e.startoff,
                    ..e
                });
            }
            out.extents.push(Extent {
                startoff: lo,
                startblock: e.startblock + (lo - e.startoff),
                blockcount: hi - lo,
                unwritten: false,
            });
            if hi < end {
                out.extents.push(Extent {
                    startoff: hi,
                    startblock: e.startblock + (hi - e.startoff),
                    blockcount: end - hi,
                    unwritten: true,
                });
            }
        } else {
            out.extents.push(e);
        }
        for fb in lo..hi {
            let phys = e.startblock + (fb - e.startoff);
            out.mapped.push((
                fb,
                if e.unwritten {
                    Block::Whole(phys)
                } else {
                    Block::InPlace(phys)
                },
            ));
        }
        cursor = cursor.max(end);
    }
    if cursor <= last {
        out.holes.push((cursor, last + 1 - cursor));
    }
    out
}

impl Filesystem {
    /// Write `data` at byte `offset` of the regular file `ino`, through
    /// the log: holes are given blocks, unwritten extents become written
    /// ones where the write covers them, and the file grows if the write
    /// ends past it.
    ///
    /// Returns the sequence number the record was given.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] for anything but a regular file, and
    /// [`Error::UnsupportedFeature`] for each of the shapes listed in
    /// this module's documentation. Every refusal comes before anything
    /// is written.
    pub fn write(&self, ino: u64, offset: u64, data: &[u8]) -> Result<u64> {
        let device = self.writable_device()?;
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "writing allocates v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if data.is_empty() {
            return Err(Error::UnsupportedFeature(
                "a write of no bytes changes nothing and has nothing to log".into(),
            ));
        }
        let end = offset.checked_add(data.len() as u64).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "a write of {} bytes at {offset} ends past a 64-bit offset",
                data.len()
            ))
        })?;
        let bs = u64::from(self.sb.blocksize);
        let first = offset / bs;
        let last = (end - 1) / bs;
        if last > MAX_FILE_BLOCK {
            return Err(Error::UnsupportedFeature(format!(
                "a write ending at byte {end} reaches file block {last}, past the last one \
                 an extent can map"
            )));
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
        if file.has_shared_extents() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} has reflinked extents; writing one would change what another \
                 inode reads"
            )));
        }
        match file.format {
            Format::Extents => {}
            Format::Btree => {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} lists its extents in a B+tree, which this write does not \
                     edit"
                )))
            }
            _ => {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} stores its data inside the inode"
                )))
            }
        }

        let old = self.data_extents(&file, &raw)?;
        let mut planned = plan(&old, first, last);
        let pieces: u64 = planned
            .holes
            .iter()
            .map(|&(_, len)| len.div_ceil(MAX_EXTENT_BLOCKS))
            .sum();
        let count = planned.extents.len() as u64 + pieces;
        let (fork_start, fork_end) = file.data_fork_range(usize::from(self.sb.inodesize));
        let room = ((fork_end - fork_start) / EXTENT_BYTES) as u64;
        if count > room {
            return Err(Error::UnsupportedFeature(format!(
                "the write leaves inode {ino} with {count} extents, more than the {room} its \
                 fork has room to list; a B+tree would be needed"
            )));
        }

        // Every hole is given blocks in the inode's own group.
        let (agno, _, _) = self.sb.split_ino(ino);
        let mut group = crate::group_write::GroupAlloc::open(&self.sb, self.device(), agno)?;
        let mut taken = 0u64;
        for &(start, len) in &planned.holes {
            let mut fb = start;
            while fb < start + len {
                let want = (start + len - fb).min(MAX_EXTENT_BLOCKS);
                let agblock = group.take(want as u32, ino as i64, fb)?;
                let fsblock = (u64::from(agno) << self.sb.agblklog) | u64::from(agblock);
                planned.extents.push(Extent {
                    startoff: fb,
                    startblock: fsblock,
                    blockcount: want,
                    unwritten: false,
                });
                for i in 0..want {
                    planned.mapped.push((fb + i, Block::Whole(fsblock + i)));
                }
                taken += want;
                fb += want;
            }
        }
        planned.extents.sort_by_key(|e| e.startoff);
        let group_items = group.into_items()?;
        let quota_items = crate::quota::accounting_items(
            self,
            &[crate::quota::QuotaChange {
                uid: file.uid,
                gid: file.gid,
                project_id: crate::quota::project_id(&raw),
                blocks_fs: i64::try_from(taken).map_err(|_| {
                    Error::UnsupportedFeature("quota block delta overflowed".into())
                })?,
                inodes: 0,
            }],
        )?;
        let mut fork = Vec::with_capacity(planned.extents.len() * EXTENT_BYTES);
        for e in &planned.extents {
            fork.extend_from_slice(&e.to_bytes()?);
        }
        let core = grown_core(
            &raw,
            file.size.max(end),
            file.nblocks + taken,
            planned.extents.len() as u64,
        )?;
        let logged = log_dinode_from_disk(&core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;

        // Every refusal is behind us. The data goes down first, the
        // record last.
        let block_at = |fsblock: u64| self.sb.fsblock_offset(fsblock);
        // The old last block's tail past the old end reads as zeros only
        // while it is past the end; a write past the end makes it visible.
        if offset > file.size && file.size % bs != 0 {
            let tail_fb = file.size / bs;
            if let Some(e) = old.iter().find(|e| {
                !e.unwritten && tail_fb >= e.startoff && tail_fb < e.startoff + e.blockcount
            }) {
                let block_end = (tail_fb + 1) * bs;
                let zero_to = offset.min(block_end);
                let at = block_at(e.startblock + (tail_fb - e.startoff)) + file.size % bs;
                device.write_at(at, &vec![0u8; (zero_to - file.size) as usize])?;
            }
        }
        planned.mapped.sort_by_key(|&(fb, _)| fb);
        for &(fb, block) in &planned.mapped {
            let block_start = fb * bs;
            let lo = offset.max(block_start);
            let hi = end.min(block_start + bs);
            let src = &data[(lo - offset) as usize..(hi - offset) as usize];
            match block {
                Block::InPlace(phys) => {
                    device.write_at(block_at(phys) + (lo - block_start), src)?;
                }
                Block::Whole(phys) => {
                    let mut whole = vec![0u8; bs as usize];
                    whole[(lo - block_start) as usize..(hi - block_start) as usize]
                        .copy_from_slice(src);
                    device.write_at(block_at(phys), &whole)?;
                }
            }
        }
        device.flush()?;

        let dsize = fork.len();
        let mut fork_op = fork;
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);
        let logged_fork = fork_op[..dsize].to_vec();
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());
        let item_ops = group_items.iter().map(|i| i.op_count()).sum::<usize>()
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + 3;
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
            for item in &group_items {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
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
        })?;

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&group_items);
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        self.logged_inode(ino, &core, &logged_fork)?;
        Ok(lsn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(startoff: u64, startblock: u64, blockcount: u64, unwritten: bool) -> Extent {
        Extent {
            startoff,
            startblock,
            blockcount,
            unwritten,
        }
    }

    #[test]
    fn a_write_inside_an_unwritten_extent_splits_it_in_three() {
        let p = plan(&[ext(0, 100, 10, true)], 3, 5);
        assert_eq!(
            p.extents,
            vec![
                ext(0, 100, 3, true),
                ext(3, 103, 3, false),
                ext(6, 106, 4, true)
            ]
        );
        assert!(p.holes.is_empty());
        assert_eq!(p.mapped.len(), 3);
        assert!(p.mapped.iter().all(|&(_, b)| matches!(b, Block::Whole(_))));
    }

    #[test]
    fn holes_are_found_before_between_and_after_extents() {
        let p = plan(&[ext(2, 50, 2, false), ext(6, 60, 1, false)], 0, 9);
        assert_eq!(p.holes, vec![(0, 2), (4, 2), (7, 3)]);
        assert_eq!(p.extents.len(), 2, "written extents are kept as they are");
        assert!(p
            .mapped
            .iter()
            .all(|&(_, b)| matches!(b, Block::InPlace(_))));
    }

    #[test]
    fn extents_outside_the_range_are_untouched() {
        let p = plan(&[ext(0, 10, 1, false), ext(20, 30, 5, true)], 5, 6);
        assert_eq!(p.extents, vec![ext(0, 10, 1, false), ext(20, 30, 5, true)]);
        assert_eq!(p.holes, vec![(5, 2)]);
        assert!(p.mapped.is_empty());
    }
}
