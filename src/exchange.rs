//! Exchanging ranges of two files' data without copying it (#374).
//!
//! Each file's range keeps its place in the file and takes the other
//! file's blocks: the extents mapping one range are moved, offsets
//! shifted, into the other file's list, and the other way round. Holes
//! and unwritten extents go with them, as they would through the kernel's
//! `XFS_IOC_EXCHANGE_RANGE`, and no data block is read or written.
//!
//! What moves with the mappings, all in one checkpoint:
//!
//! - **both inodes**: their extent lists, block counts and change times;
//!   sizes do not change. A list is laid out inline or as a block-map
//!   B+tree by the kernel's rule, whichever form it had before, so an
//!   exchange can turn either into the other (#441);
//! - **the reverse map**, where the volume has one: each moved extent's
//!   record changes owner and file offset, and a block-map tree rebuilt
//!   gives back its old blocks and takes new ones;
//! - **quota**, where the two files have different owners: each is
//!   charged for the blocks it now holds.
//!
//! # Shared blocks
//!
//! A reflinked block keeps the same number of mappings through an
//! exchange: each mapping changes file, none is added or dropped. So the
//! reference-count tree does not change (#441). The reverse map does, and
//! there a shared block has one record per mapping, overlapping, which a
//! moved record may now join. A file that takes a shared block takes the
//! reflink flag with it: both files carry it after an exchange when
//! either did, as the kernel leaves them.
//!
//! The kernel logs an exchange as intent items (`XMI`/`XMD`), so recovery
//! can finish one a crash interrupted. This driver writes the whole
//! exchange as one checkpoint, which recovery applies whole or not at
//! all, so it needs no intent and writes none.
//!
//! # What it will not do
//!
//! Each is refused by name before anything is written: a volume without
//! the exchange-range feature (the kernel refuses that too), a range that
//! is not whole blocks or runs past either file's end, one file with
//! itself, anything but two regular files, and real-time or inline files.

use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::fs::Filesystem;
use crate::group_write::Allocations;
use crate::inode::{flags2, offsets as at, Format, Inode};
use crate::log_write::{
    inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer,
    Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};
use crate::rmap::{Rmap, OFF_UNWRITTEN};
use crate::superblock::incompat;

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// `extents` cut at file blocks `first` and `first + n`: the pieces inside
/// the range, and every piece outside it.
fn cut(extents: &[Extent], first: u64, n: u64) -> (Vec<Extent>, Vec<Extent>) {
    let end = first + n;
    let (mut inside, mut outside) = (Vec::new(), Vec::new());
    for e in extents {
        let (lo, hi) = (e.startoff, e.startoff + e.blockcount);
        let piece = |from: u64, to: u64| Extent {
            startoff: from,
            startblock: e.startblock + (from - lo),
            blockcount: to - from,
            unwritten: e.unwritten,
        };
        if lo < first {
            outside.push(piece(lo, hi.min(first)));
        }
        if lo.max(first) < hi.min(end) {
            inside.push(piece(lo.max(first), hi.min(end)));
        }
        if hi > end {
            outside.push(piece(lo.max(end), hi));
        }
    }
    (inside, outside)
}

/// `pieces` moved from file block `from` to file block `to`.
fn shifted(pieces: &[Extent], from: u64, to: u64) -> Vec<Extent> {
    pieces
        .iter()
        .map(|e| Extent {
            startoff: e.startoff - from + to,
            ..*e
        })
        .collect()
}

/// One file's extents in order, neighbours that continue each other on
/// disk and in the file joined, as the kernel keeps them.
fn joined(mut extents: Vec<Extent>) -> Vec<Extent> {
    extents.sort_by_key(|e| e.startoff);
    let mut out: Vec<Extent> = Vec::with_capacity(extents.len());
    for e in extents {
        if let Some(last) = out.last_mut() {
            if last.unwritten == e.unwritten
                && last.startoff + last.blockcount == e.startoff
                && last.startblock + last.blockcount == e.startblock
                && last.blockcount + e.blockcount <= crate::write_alloc::MAX_EXTENT_BLOCKS
            {
                last.blockcount += e.blockcount;
                continue;
            }
        }
        out.push(e);
    }
    out
}

fn blocks_in(extents: &[Extent]) -> u64 {
    extents.iter().map(|e| e.blockcount).sum()
}

/// The reverse-map record for `piece` of `owner`'s data fork.
fn rmap_of(fs: &Filesystem, piece: &Extent, owner: u64) -> Result<(u32, Rmap)> {
    let (agno, agbno) = fs.sb.split_fsblock(piece.startblock);
    let count = u32::try_from(piece.blockcount)
        .map_err(|_| Error::UnsupportedFeature("an extent longer than a group".into()))?;
    let flag = if piece.unwritten { OFF_UNWRITTEN } else { 0 };
    Ok((
        agno,
        Rmap {
            startblock: agbno,
            blockcount: count,
            owner: owner as i64,
            offset: piece.startoff | flag,
        },
    ))
}

impl Filesystem {
    /// Exchange `length` bytes of inode `a` at `a_offset` with as many of
    /// inode `b` at `b_offset`, by exchanging the blocks that map them.
    ///
    /// Offsets and length are in bytes and whole blocks. Each range has to
    /// lie inside its file, counting the last block whole. Returns the
    /// sequence number of the record the exchange was written in.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotAFile`] when either inode is not a regular file, and
    /// [`Error::UnsupportedFeature`] for each refusal in this module's
    /// documentation. Every refusal comes before anything is written.
    pub fn exchange_range(
        &self,
        a: u64,
        a_offset: u64,
        b: u64,
        b_offset: u64,
        length: u64,
    ) -> Result<u64> {
        self.writable_device()?;
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "an exchange writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if self.sb.features_incompat & incompat::EXCHRANGE == 0 {
            return Err(Error::UnsupportedFeature(
                "this volume does not have the exchange-range feature (exchrange), which an \
                 exchange needs, as it does in the kernel"
                    .into(),
            ));
        }
        if a == b {
            return Err(Error::UnsupportedFeature(format!(
                "inode {a} cannot exchange a range with itself"
            )));
        }
        let bs = u64::from(self.sb.blocksize);
        if length == 0 {
            return Err(Error::UnsupportedFeature(
                "an exchange of no bytes changes nothing".into(),
            ));
        }
        if !a_offset.is_multiple_of(bs)
            || !b_offset.is_multiple_of(bs)
            || !length.is_multiple_of(bs)
        {
            return Err(Error::UnsupportedFeature(format!(
                "an exchange moves whole blocks of {bs} bytes; {length} bytes at {a_offset} and \
                 {b_offset} are not"
            )));
        }
        let (fa, fb, n) = (a_offset / bs, b_offset / bs, length / bs);

        let (ia, raw_a) = self.read_inode_raw(a)?;
        let (ib, raw_b) = self.read_inode_raw(b)?;
        for (inode, first) in [(&ia, fa), (&ib, fb)] {
            refuse_unexchangeable(inode)?;
            let blocks = inode.size.div_ceil(bs);
            if first + n > blocks {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}: blocks {first}..{} run past its {} bytes",
                    inode.ino,
                    first + n,
                    inode.size
                )));
            }
        }

        let old_a = self.data_extents(&ia, &raw_a)?;
        let old_b = self.data_extents(&ib, &raw_b)?;
        let (in_a, out_a) = cut(&old_a, fa, n);
        let (in_b, out_b) = cut(&old_b, fb, n);
        let mut new_a = out_a;
        new_a.extend(shifted(&in_b, fb, fa));
        let new_a = joined(new_a);
        let mut new_b = out_b;
        new_b.extend(shifted(&in_a, fa, fb));
        let new_b = joined(new_b);
        // The reverse map: every moved piece leaves its old owner's record
        // and joins the new owner's, at its new offset. All the records
        // come out before any go in, so a group's tree never holds one
        // mapping twice.
        let mut allocations = Allocations::new();
        if self.sb.has_rmapbt() {
            let mut moves = Vec::new();
            for (pieces, from, to, owner, taker) in [(&in_a, fa, fb, a, b), (&in_b, fb, fa, b, a)] {
                for piece in pieces.iter() {
                    let moved = Extent {
                        startoff: piece.startoff - from + to,
                        ..*piece
                    };
                    moves.push((rmap_of(self, piece, owner)?, rmap_of(self, &moved, taker)?));
                }
            }
            for ((agno, old), _) in &moves {
                allocations
                    .group(&self.sb, self.device(), *agno)?
                    .forget_rmap(*old)?;
            }
            for (_, (agno, new)) in &moves {
                allocations
                    .group(&self.sb, self.device(), *agno)?
                    .remember_moved_rmap(*new)?;
            }
        }
        // Each list laid out in the form its length needs, the tree each
        // had given back.
        let map_a = self.map_data_fork(&mut allocations, a, &ia, &raw_a, &new_a)?;
        let map_b = self.map_data_fork(&mut allocations, b, &ib, &raw_b, &new_b)?;
        let group_items = allocations.into_items()?;

        // Each file's blocks: what it mapped, less what it gave, plus what
        // it took, with its tree's blocks swapped for the new tree's.
        let (moved_a, moved_b) = (blocks_in(&in_a), blocks_in(&in_b));
        let held = |inode: &Inode, map: &crate::bmbt_write::ForkMap, gave: u64, took: u64| {
            inode
                .nblocks
                .checked_sub(gave + map.old_tree_blocks)
                .map(|n| n + took + map.tree_blocks)
                .ok_or_else(|| {
                    Error::BadSuperblock(format!(
                        "inode {} counts {} blocks, fewer than the {} it maps in the range and \
                         the {} of its block map",
                        inode.ino, inode.nblocks, gave, map.old_tree_blocks
                    ))
                })
        };
        let blocks_a = held(&ia, &map_a, moved_a, moved_b)?;
        let blocks_b = held(&ib, &map_b, moved_b, moved_a)?;
        let delta = |now: u64, before: u64| -> Result<i64> {
            i64::try_from(now)
                .and_then(|n| i64::try_from(before).map(|b| n - b))
                .map_err(|_| Error::UnsupportedFeature("quota block delta overflowed".into()))
        };
        let quota_items = crate::quota::accounting_items(
            self,
            &[
                crate::quota::QuotaChange {
                    uid: ia.uid,
                    gid: ia.gid,
                    project_id: crate::quota::project_id(&raw_a),
                    blocks_fs: delta(blocks_a, ia.nblocks)?,
                    inodes: 0,
                },
                crate::quota::QuotaChange {
                    uid: ib.uid,
                    gid: ib.gid,
                    project_id: crate::quota::project_id(&raw_b),
                    blocks_fs: delta(blocks_b, ib.nblocks)?,
                    inodes: 0,
                },
            ],
        )?;

        let reflink = ia.has_shared_extents() || ib.has_shared_extents();
        let core = |raw: &[u8],
                    inode: &Inode,
                    blocks: u64,
                    list: &[Extent],
                    map: &crate::bmbt_write::ForkMap| {
            let mut core =
                crate::write_alloc::grown_core(raw, inode.size, blocks, list.len() as u64)?;
            core[at::FORMAT] = map.format as u8;
            if reflink {
                let flags = u64::from_be_bytes(
                    core[at::FLAGS2..at::FLAGS2 + 8]
                        .try_into()
                        .expect("8 bytes"),
                ) | flags2::REFLINK;
                core[at::FLAGS2..at::FLAGS2 + 8].copy_from_slice(&flags.to_be_bytes());
            }
            Ok::<_, Error>(core)
        };
        let logged = [
            (a, core(&raw_a, &ia, blocks_a, &new_a, &map_a)?, &map_a),
            (b, core(&raw_b, &ib, blocks_b, &new_b, &map_b)?, &map_b),
        ];
        let mut inode_ops = Vec::new();
        for (ino, core, map) in &logged {
            let buffer =
                InodeBuffer::containing(self.inode_offset(*ino)?, self.sb.inode_cluster_bytes());
            let dinode = log_dinode_from_disk(core)
                .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
            if map.logged.is_empty() {
                inode_ops.push(inode_log_format(*ino, XFS_ILOG_CORE, &buffer));
                inode_ops.push(dinode);
            } else {
                let mut op = map.logged.clone();
                op.resize(map.logged.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
                inode_ops.push(inode_log_format_with_fork(
                    *ino,
                    XFS_ILOG_CORE | map.fields,
                    &buffer,
                    map.logged.len() as u16,
                ));
                inode_ops.push(dinode);
                inode_ops.push(op);
            }
        }

        let tree_items: Vec<_> = map_a.items.iter().chain(&map_b.items).cloned().collect();
        let item_ops = group_items.iter().map(|i| i.op_count()).sum::<usize>()
            + tree_items.iter().map(|i| i.op_count()).sum::<usize>()
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + inode_ops.len();
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
            for item in group_items.iter().chain(&tree_items) {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            for data in &inode_ops {
                ops.push(Op {
                    flags: 0,
                    data: data.clone(),
                });
            }
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&group_items);
        self.logged_buffers(&tree_items);
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        for (ino, core, map) in &logged {
            self.logged_inode(*ino, core, &map.fork)?;
        }
        Ok(lsn)
    }
}

/// An inode whose data this exchange does not move, refused by name.
fn refuse_unexchangeable(inode: &Inode) -> Result<()> {
    let ino = inode.ino;
    if !inode.is_regular_file() {
        return Err(Error::NotAFile);
    }
    if inode.is_realtime() {
        return Err(Error::UnsupportedFeature(format!(
            "inode {ino} keeps its data on the real-time device"
        )));
    }
    match inode.format {
        Format::Extents | Format::Btree => Ok(()),
        _ => Err(Error::UnsupportedFeature(format!(
            "inode {ino} stores its data inside the inode"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ext(startoff: u64, startblock: u64, blockcount: u64) -> Extent {
        Extent {
            startoff,
            startblock,
            blockcount,
            unwritten: false,
        }
    }

    #[test]
    fn an_extent_across_both_edges_is_cut_in_three() {
        let (inside, outside) = cut(&[ext(0, 100, 10)], 3, 4);
        assert_eq!(inside, vec![ext(3, 103, 4)]);
        assert_eq!(outside, vec![ext(0, 100, 3), ext(7, 107, 3)]);
    }

    #[test]
    fn extents_wholly_inside_or_outside_are_not_cut() {
        let (inside, outside) = cut(&[ext(0, 10, 2), ext(4, 20, 2), ext(9, 30, 1)], 3, 5);
        assert_eq!(inside, vec![ext(4, 20, 2)]);
        assert_eq!(outside, vec![ext(0, 10, 2), ext(9, 30, 1)]);
    }

    #[test]
    fn neighbours_continuing_each_other_are_joined() {
        let joined = joined(vec![ext(2, 52, 1), ext(0, 50, 2), ext(5, 90, 1)]);
        assert_eq!(joined, vec![ext(0, 50, 3), ext(5, 90, 1)]);
    }
}
