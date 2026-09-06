//! Removing a file, through the log.
//!
//! Create in reverse, and the same five items: the group's inode header,
//! both inode trees, the parent directory and the inode itself. The name
//! goes out of the directory, the inode goes back into the group's
//! accounting, and the inode is emptied.
//!
//! # The case that is easy to get wrong
//!
//! Giving an inode back to a chunk that had **none** free puts that
//! chunk into the free-inode tree, which is a change of membership
//! rather than of contents — the mirror of a create taking a chunk's
//! last free inode and pushing it out.
//!
//! A driver that updated the counts and left the tree alone would not
//! corrupt anything, and nothing would report it. The filesystem would
//! simply lose an inode: free, correctly recorded as free, and invisible
//! to the tree a create looks in. That is why the fixtures cover it and
//! why the test says which case each one exercised.
//!
//! # What the kernel writes into a freed inode
//!
//! Read off a filesystem before and after `rm`: the magic and the
//! version stay, the mode and the link count go to zero, and the
//! **generation changes** — it read 0 before and 4,245,130,214 after.
//!
//! The generation is what stops a reference to the inode's previous life
//! from resolving to whatever is put there next, so it has to move. The
//! kernel randomises it; this increments it, because there is no
//! entropy here to randomise with and inventing some would be worse than
//! saying so. Incrementing gives the property that matters — the new
//! generation is not the old one — and does not give unpredictability.
//! A driver serving NFS handles to a hostile network would want the
//! stronger of the two.
//!
//! # What it will not do
//!
//! Each is refused by name rather than attempted:
//!
//! - a file that still holds blocks, which would have to free extents as
//!   well and is a bigger transaction than this one;
//! - a file with more than one link, where the inode survives and only
//!   the count moves;
//! - a directory, which has `.` and `..` to account for and a parent
//!   whose link count changes;
//! - a parent that has outgrown its inode;
//! - inode trees more than one level deep, or a root with no room for
//!   the chunk this may put back;
//! - a v4 filesystem.

use crate::dir;
use crate::error::{Error, Result};
use crate::format::log_items::inode_log_format::XFS_ILOG_DDATA;
use crate::fs::Filesystem;
use crate::inode::Format;
use crate::log_write::{
    append, inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header,
    InodeBuffer, Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; a fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that a removal changes.
mod core_at {
    pub const MODE: usize = 2;
    pub const NLINK: usize = 16;
    pub const SIZE: usize = 56;
    pub const GEN: usize = 92;
    pub const CHANGECOUNT: usize = 104;
}

/// The inode core of a file that has just been removed.
///
/// The identity fields are left exactly as they are: this inode will be
/// handed out again, and `di_ino` and `di_uuid` are as correct now as
/// they will be then.
fn emptied_core(raw: &[u8]) -> Vec<u8> {
    let mut core = raw.to_vec();
    core[core_at::MODE..core_at::MODE + 2].copy_from_slice(&0u16.to_be_bytes());
    core[core_at::NLINK..core_at::NLINK + 4].copy_from_slice(&0u32.to_be_bytes());
    core[core_at::SIZE..core_at::SIZE + 8].copy_from_slice(&0u64.to_be_bytes());

    // See the note at the top on why this increments where the kernel
    // randomises.
    let gen = u32::from_be_bytes(
        core[core_at::GEN..core_at::GEN + 4]
            .try_into()
            .expect("4 bytes"),
    );
    core[core_at::GEN..core_at::GEN + 4].copy_from_slice(&gen.wrapping_add(1).to_be_bytes());

    let at = core_at::CHANGECOUNT;
    let now = u64::from_be_bytes(core[at..at + 8].try_into().expect("8 bytes"));
    core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
    core
}

impl Filesystem {
    /// Remove `name` from `parent`, freeing the inode it names.
    ///
    /// Returns the removed file's inode number and the sequence number
    /// the record was given. Nothing on disk is touched: the record is
    /// the change.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotFound`] if the name is not there,
    /// [`Error::NotADirectory`] if `parent` is not one, and
    /// [`Error::UnsupportedFeature`] for each of the shapes listed in
    /// this module's documentation.
    pub fn unlink_file(&self, parent: u64, name: &[u8]) -> Result<(u64, u64)> {
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "removing writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }

        let (dir_inode, dir_raw) = self.read_inode_raw(parent)?;
        if !dir_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if dir_inode.format != Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {parent} has outgrown the inode, so removing an entry rewrites a \
                 directory block rather than the inode's own fork"
            )));
        }

        let (fork_start, fork_end) = dir_inode.data_fork_range(usize::from(self.sb.inodesize));
        let parsed = dir::read_short_form(&dir_inode, &dir_raw[fork_start..fork_end], &self.sb)?;
        let entry = parsed
            .entries
            .iter()
            .find(|e| e.name == name)
            .ok_or(Error::NotFound)?;
        let ino = entry.ino;

        let (victim, victim_raw) = self.read_inode_raw(ino)?;
        if victim.is_dir() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} is a directory, which has `.` and `..` to account for and a \
                 parent whose link count changes; only a regular file is supported"
            )));
        }
        if victim.nlink != 1 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} has {} links, so removing this name leaves the inode alive \
                 and only moves the count",
                victim.nlink
            )));
        }
        if victim.nblocks != 0 || victim.nextents != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} still holds {} blocks in {} extents, which would have to be \
                 freed as well; truncate it first",
                victim.nblocks, victim.nextents
            )));
        }

        let (agno, _, _) = self.sb.split_ino(ino);

        // ONE EDITOR FOR THE GROUP'S INODE TREES, at whatever depth they
        // are. This read the AGI, checked both trees were a single block
        // deep, edited a chunk and wrote the roots back. A 1 KiB root
        // holds 60 chunk records and a chunk is 64 inodes, so a group
        // with four thousand inodes in it already has a deeper tree and
        // could not be unlinked from.
        let mut trees = crate::inode_btree::Trees::open(&self.sb, self.device(), agno)?;

        // Which chunk holds it, and where in that chunk.
        let (_, ag_block, offset) = self.sb.split_ino(ino);
        let agino = (ag_block << self.sb.inopblog) | offset;
        let index = trees
            .chunks()
            .iter()
            .position(|c| {
                agino >= c.startino
                    && agino - c.startino < u32::from(crate::inode_btree::INODES_PER_CHUNK)
            })
            .ok_or_else(|| {
                Error::CorruptLog(format!(
                    "inode {ino} is in no chunk of allocation group {agno}'s inode tree"
                ))
            })?;
        let slot = (agino - trees.chunks()[index].startino) as u8;
        trees.chunks_mut()[index].give_back(slot)?;

        // The count of allocated inodes does not move: freeing one
        // inside a chunk leaves the chunk where it was, and the count is
        // of chunks' worth of inodes rather than of inodes in use.
        let count = trees.agi().count;
        let freecount: u32 = trees.chunks().iter().map(|c| u32::from(c.freecount)).sum();
        trees.set_counts(count, freecount, None);
        let group_items = trees.into_items()?;

        let fork = self.short_form_without_entry(&parsed, name, fork_end - fork_start)?;
        let mut dir_core = dir_raw.clone();
        dir_core[core_at::SIZE..core_at::SIZE + 8]
            .copy_from_slice(&(fork.len() as u64).to_be_bytes());
        let at = core_at::CHANGECOUNT;
        let now = u64::from_be_bytes(dir_core[at..at + 8].try_into().expect("8 bytes"));
        dir_core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());

        let victim_core = emptied_core(&victim_raw);

        let dir_logged = log_dinode_from_disk(&dir_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {parent}: {why}")))?;
        let victim_logged = log_dinode_from_disk(&victim_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;

        let cluster = self.sb.inode_cluster_bytes();
        let dir_buf = InodeBuffer::containing(self.inode_offset(parent)?, cluster);
        let victim_buf = InodeBuffer::containing(self.inode_offset(ino)?, cluster);

        let dsize = fork.len();
        let mut fork_op = fork;
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);

        let item_ops = group_items.iter().map(|i| i.op_count()).sum::<usize>() + 3 + 2;

        // Every refusal this operation has is behind us and the next
        // statement writes, so the mount's one checkpoint is claimed
        // here rather than on the way in: a refusal must not spend it.
        // See `Filesystem::begin_checkpoint`.
        self.begin_checkpoint()?;
        let lsn = append(device.as_ref(), &self.sb, |tid| {
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
                data: inode_log_format_with_fork(
                    parent,
                    XFS_ILOG_CORE | XFS_ILOG_DDATA,
                    &dir_buf,
                    dsize as u16,
                ),
            });
            ops.push(Op {
                flags: 0,
                data: dir_logged,
            });
            ops.push(Op {
                flags: 0,
                data: fork_op,
            });
            ops.push(Op {
                flags: 0,
                data: inode_log_format(ino, XFS_ILOG_CORE, &victim_buf),
            });
            ops.push(Op {
                flags: 0,
                data: victim_logged,
            });
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        Ok((ino, lsn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A removed file has no mode, no links and no size, and its
    /// generation has moved on.
    #[test]
    fn an_emptied_core_is_a_free_inode_again() {
        let mut raw = vec![0u8; 176];
        raw[core_at::MODE..core_at::MODE + 2].copy_from_slice(&0o100644u16.to_be_bytes());
        raw[core_at::NLINK..core_at::NLINK + 4].copy_from_slice(&1u32.to_be_bytes());
        raw[core_at::SIZE..core_at::SIZE + 8].copy_from_slice(&4096u64.to_be_bytes());
        raw[core_at::GEN..core_at::GEN + 4].copy_from_slice(&41u32.to_be_bytes());

        let core = emptied_core(&raw);
        assert_eq!(
            u16::from_be_bytes(core[core_at::MODE..core_at::MODE + 2].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_be_bytes(core[core_at::NLINK..core_at::NLINK + 4].try_into().unwrap()),
            0
        );
        assert_eq!(
            u64::from_be_bytes(core[core_at::SIZE..core_at::SIZE + 8].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_be_bytes(core[core_at::GEN..core_at::GEN + 4].try_into().unwrap()),
            42,
            "the generation must move on, so a reference to the inode's previous life \
             cannot resolve to whatever is put there next"
        );
    }

    /// The identity fields survive, because this inode will be handed
    /// out again and they are as correct now as they will be then.
    #[test]
    fn the_identity_fields_survive() {
        const DI_INO: usize = 152;
        const DI_UUID: usize = 160;

        let mut raw = vec![0u8; 176];
        raw[0..2].copy_from_slice(&0x494eu16.to_be_bytes());
        raw[4] = 3;
        raw[DI_INO..DI_INO + 8].copy_from_slice(&186u64.to_be_bytes());
        raw[DI_UUID..DI_UUID + 16].copy_from_slice(&[0xcd; 16]);

        let core = emptied_core(&raw);
        assert_eq!(&core[0..2], &0x494eu16.to_be_bytes(), "di_magic");
        assert_eq!(core[4], 3, "di_version");
        assert_eq!(
            u64::from_be_bytes(core[DI_INO..DI_INO + 8].try_into().unwrap()),
            186,
            "di_ino"
        );
        assert_eq!(&core[DI_UUID..DI_UUID + 16], &[0xcd; 16], "di_uuid");
    }
}
