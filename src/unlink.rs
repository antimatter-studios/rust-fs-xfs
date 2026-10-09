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
//! - inode trees more than one level deep, or a root with no room for
//!   the chunk this may put back;
//! - a v4 filesystem.

use crate::create::clock_now;
use crate::dir;
use crate::error::{Error, Result};
use crate::format::log_items::inode_log_format::XFS_ILOG_DDATA;
use crate::fs::Filesystem;
use crate::inode::{stamp_change, Changed, Format};
use crate::log_write::{
    inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer,
    Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; a fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that a removal changes.
mod core_at {
    pub const MODE: usize = 2;
    /// `di_format`: how the data fork maps its blocks.
    pub const FORMAT: usize = 5;
    pub const NLINK: usize = 16;
    /// `di_format`: how the data fork is kept.
    pub const FORMAT: usize = 5;
    pub const SIZE: usize = 56;
    /// `di_nblocks`: the blocks the inode owns.
    pub const NBLOCKS: usize = 64;
    pub const GEN: usize = 92;
    pub const CHANGECOUNT: usize = 104;
    /// `di_nextents`, then `di_anextents`: the extent counts. Under
    /// NREXT64 the four bytes at 76 hold the attribute fork's count.
    pub const NEXTENTS: usize = 76;
    pub const FORKOFF: usize = 82;
    pub const AFORMAT: usize = 83;
}

/// The inode core of a file that has just been removed.
///
/// The identity fields are left exactly as they are: this inode will be
/// handed out again, and `di_ino` and `di_uuid` are as correct now as
/// they will be then.
pub(crate) fn emptied_core(raw: &[u8]) -> Vec<u8> {
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

    // AS `xfs_ifree` LEAVES IT (#189): no flags, and no attribute fork.
    // A local fork holds no blocks, so a file with attributes passes the
    // "holds no blocks" refusal, and its fork stayed in the free inode.
    // The attribute extent count is the u16 at 80, or, under NREXT64, the
    // u32 at 76.
    //
    // AND NO BLOCKS, MAPPED AS NO EXTENTS. A rename frees a file that
    // still held blocks when its core was read, and gives them back in
    // the same checkpoint, so the count goes with them; `xfs_dinode_verify`
    // refuses an inode with no extents and blocks still counted, whatever
    // its mode, and log recovery stopped on it (#383). The data fork is
    // an empty extent list, as `xfs_ifree` leaves every freed inode.
    set_nextents(&mut core, 0);
    crate::create::reset_flags(&mut core);
    core[core_at::FORKOFF] = 0;
    core[core_at::AFORMAT] = AFORMAT_EXTENTS;
    core[core_at::FORMAT] = AFORMAT_EXTENTS;
    core[core_at::NEXTENTS..core_at::FORKOFF].fill(0);
    core[core_at::NBLOCKS..core_at::NBLOCKS + 8].fill(0);
    core
}

/// `XFS_DINODE_FMT_EXTENTS`, the format of an empty fork of either kind.
const AFORMAT_EXTENTS: u8 = 2;

/// Set `di_nextents`, wherever the inode's own feature bits put it.
fn set_nextents(core: &mut [u8], count: u64) {
    const NEXTENTS: usize = 76;
    const NEXTENTS64: usize = 24;
    const FLAGS2: usize = 120;
    let nrext64 = u64::from_be_bytes(core[FLAGS2..FLAGS2 + 8].try_into().expect("8 bytes"))
        & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
        != 0;
    if nrext64 {
        core[NEXTENTS64..NEXTENTS64 + 8].copy_from_slice(&count.to_be_bytes());
    } else {
        core[NEXTENTS..NEXTENTS + 4].copy_from_slice(&(count as u32).to_be_bytes());
    }
}

/// What a removal expects the name to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    File,
    Directory,
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
        self.remove_name(parent, name, Target::File)
    }

    /// Remove the empty directory `name` from `parent` (#385).
    ///
    /// The same transaction as [`Filesystem::unlink_file`], with two more
    /// things kept right: the directory's `.` and `..` go with its inode,
    /// and its `..` was a link to `parent`, so the parent's link count
    /// falls by one.
    ///
    /// Returns the removed directory's inode number and the sequence
    /// number the record was given.
    ///
    /// # Errors
    ///
    /// [`Error::DirectoryNotEmpty`] if the directory still holds a name,
    /// [`Error::NotADirectory`] if `name` is not a directory or `parent`
    /// is not one, [`Error::NotFound`] if the name is not there, and the
    /// rest as [`Filesystem::unlink_file`]. Every refusal comes before
    /// anything is written.
    pub fn remove_directory(&self, parent: u64, name: &[u8]) -> Result<(u64, u64)> {
        self.remove_name(parent, name, Target::Directory)
    }

    /// The inode-tree items that give inode `ino` back to its chunk: the
    /// chunk's free mask and count, both inode trees, and the group's
    /// header. Nothing is written.
    pub(crate) fn freed_inode_items(&self, ino: u64) -> Result<Vec<crate::buf_write::BufferItem>> {
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
        trees.into_items()
    }

    fn remove_name(&self, parent: u64, name: &[u8], target: Target) -> Result<(u64, u64)> {
        self.writable_device()?;
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "removing writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }

        let (dir_inode, dir_raw) = self.read_inode_raw(parent)?;
        if !dir_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        let (fork_start, fork_end) = dir_inode.data_fork_range(usize::from(self.sb.inodesize));
        // A directory in its inode loses the name from its fork; one past
        // its inode loses it in whichever form it is in (#366, #367).
        let (parsed, in_blocks) = match dir_inode.format {
            Format::Local => (
                Some(dir::read_short_form(
                    &dir_inode,
                    &dir_raw[fork_start..fork_end],
                    &self.sb,
                )?),
                false,
            ),
            Format::Extents | Format::Btree => (None, true),
            other => {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {parent} keeps its entries in {other:?} form, which removing an \
                     entry here does not understand"
                )))
            }
        };
        // Found through the hash index past the inode, which never names
        // `.` or `..`.
        let ino = match &parsed {
            Some(p) => p
                .entries
                .iter()
                .find(|e| e.name == name)
                .map(|e| e.ino)
                .ok_or(Error::NotFound)?,
            None => self.lookup(&dir_inode, &dir_raw, name)?.ino,
        };

        let (victim, victim_raw) = self.read_inode_raw(ino)?;
        match target {
            Target::File => {
                if victim.is_dir() {
                    return Err(Error::UnsupportedFeature(format!(
                        "inode {ino} is a directory; remove_directory removes one, and \
                         unlink_file only a regular file"
                    )));
                }
            }
            Target::Directory => {
                if !victim.is_dir() {
                    return Err(Error::NotADirectory);
                }
                if !self.read_dir(&victim, &victim_raw)?.is_empty() {
                    return Err(Error::DirectoryNotEmpty);
                }
                // Its own `.` and the parent's entry naming it. Anything
                // else is a count this does not know how to account for.
                if victim.nlink != 2 {
                    return Err(Error::UnsupportedFeature(format!(
                        "empty directory inode {ino} has {} links, not the 2 its own `.` \
                         and its parent's entry make",
                        victim.nlink
                    )));
                }
            }
        }
        // A file with another name keeps living, a link fewer (#384); its
        // last name, or a directory, frees it with everything it owns.
        let freed = target == Target::Directory || victim.nlink <= 1;
        if freed {
            if victim.format == Format::Btree {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} keeps its extents in a B+tree, which freeing it here does \
                     not undo"
                )));
            }
            if victim.anextents > 0 && victim.aformat != Format::Local {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino} has attribute blocks, which freeing it here does not give \
                     back"
                )));
            }
        }

        let group_items = if freed {
            self.freed_inode_items(ino)?
        } else {
            Vec::new()
        };

        let mut allocations = crate::group_write::Allocations::new();
        let (fork, disk_fork, dir_flags, dir_format, dir_size, dir_blocks, dir_nextents, dir_items) =
            match (&parsed, in_blocks) {
                (Some(p), _) => {
                    let fork = self.short_form_without_entry(p, name, fork_end - fork_start)?;
                    let size = fork.len() as u64;
                    (
                        fork.clone(),
                        fork,
                        XFS_ILOG_DDATA,
                        Format::Local,
                        size,
                        dir_inode.nblocks,
                        dir_inode.nextents,
                        Vec::new(),
                    )
                }
                (None, true) => {
                    let rw = self.edit_directory(
                        &mut allocations,
                        parent,
                        &dir_inode,
                        &dir_raw,
                        &[crate::dir_edit::DirEdit::Remove(name)],
                    )?;
                    (
                        rw.logged_fork,
                        rw.fork,
                        rw.fields,
                        rw.format,
                        rw.size,
                        rw.blocks,
                        rw.nextents,
                        rw.items,
                    )
                }
                (None, false) => unreachable!("one form or the other"),
            };
        if freed && victim.format == Format::Extents {
            let extents = self.data_extents(&victim, &victim_raw)?;
            self.free_file_extents(&mut allocations, ino, &extents)?;
        }
        let allocation_items = allocations.into_items()?;
        let mut dir_core = dir_raw.clone();
        dir_core[core_at::SIZE..core_at::SIZE + 8].copy_from_slice(&dir_size.to_be_bytes());
        dir_core[core_at::FORMAT] = dir_format as u8;
        dir_core[core_at::NBLOCKS..core_at::NBLOCKS + 8].copy_from_slice(&dir_blocks.to_be_bytes());
        set_nextents(&mut dir_core, dir_nextents);
        let at = core_at::CHANGECOUNT;
        let now = u64::from_be_bytes(dir_core[at..at + 8].try_into().expect("8 bytes"));
        dir_core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
        // The directory lost an entry, and the kernel's `xfs_remove` stamps
        // its mtime and ctime with the moment it did (#279).
        stamp_change(&mut dir_core, clock_now(), Changed::Contents);
        // A removed subdirectory's `..` was a link to this one.
        if target == Target::Directory {
            let was = dir_inode.nlink;
            let now = was.checked_sub(1).filter(|&n| n >= 2).ok_or_else(|| {
                Error::UnsupportedFeature(format!(
                    "directory inode {parent} has {was} links, too few to hold a \
                     subdirectory's `..`"
                ))
            })?;
            dir_core[core_at::NLINK..core_at::NLINK + 4].copy_from_slice(&now.to_be_bytes());
        }

        let victim_core = if freed {
            emptied_core(&victim_raw)
        } else {
            let mut core = victim_raw.clone();
            core[core_at::NLINK..core_at::NLINK + 4]
                .copy_from_slice(&(victim.nlink - 1).to_be_bytes());
            let at = core_at::CHANGECOUNT;
            let now = u64::from_be_bytes(core[at..at + 8].try_into().expect("8 bytes"));
            core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
            stamp_change(&mut core, clock_now(), Changed::Status);
            core
        };
        let mut quota_changes = Vec::new();
        if freed {
            quota_changes.push(crate::quota::QuotaChange {
                uid: victim.uid,
                gid: victim.gid,
                project_id: crate::quota::project_id(&victim_raw),
                blocks_fs: -(victim.nblocks as i64),
                inodes: -1,
            });
        }
        // A directory that gave blocks back, or took them, charges its owner.
        if dir_blocks != dir_inode.nblocks {
            quota_changes.push(crate::quota::QuotaChange {
                uid: dir_inode.uid,
                gid: dir_inode.gid,
                project_id: crate::quota::project_id(&dir_raw),
                blocks_fs: dir_blocks as i64 - dir_inode.nblocks as i64,
                inodes: 0,
            });
        }
        let quota_items = crate::quota::accounting_items(self, &quota_changes)?;

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

        let item_ops = group_items.iter().map(|i| i.op_count()).sum::<usize>()
            + allocation_items.iter().map(|i| i.op_count()).sum::<usize>()
            + dir_items.iter().map(|i| i.op_count()).sum::<usize>()
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + 3
            + 2;

        // Every refusal this operation has is behind us and the next
        // statement writes, so the mount's one checkpoint is claimed
        // here rather than on the way in: a refusal must not spend it.
        // Kept for the overlay, which needs the same bytes the record
        // carries (#89).
        let logged_fork = disk_fork;
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
            for item in &allocation_items {
                ops.extend(item.ops());
            }
            for item in &dir_items {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            ops.push(Op {
                flags: 0,
                data: inode_log_format_with_fork(
                    parent,
                    XFS_ILOG_CORE | dir_flags,
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

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&group_items);
        self.logged_buffers(&allocation_items);
        self.logged_buffers(&dir_items);
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        self.logged_inode(parent, &dir_core, &logged_fork)?;
        self.logged_inode(ino, &victim_core, &[])?;

        Ok((ino, lsn))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A removed inode is reset the way the kernel's `xfs_ifree` resets it
    /// (#189): no flags, `di_flags2` back to the filesystem's defaults, and
    /// no attribute fork. Otherwise a later create that reads the free
    /// inode back hands its flags and attributes to an unrelated file.
    #[test]
    fn an_emptied_core_carries_no_flags_and_no_attribute_fork() {
        use crate::format::log_items::log_dinode::flags2::{DI_FLAGS2_BIGTIME, DI_FLAGS2_NREXT64};
        const FLAGS: usize = 90;
        const FLAGS2: usize = 120;
        const ANEXTENTS: usize = 80;
        const FORKOFF: usize = 82;
        const AFORMAT: usize = 83;
        let mut raw = vec![0u8; 176];
        raw[FLAGS..FLAGS + 2].copy_from_slice(&0x0018u16.to_be_bytes());
        raw[FLAGS2..FLAGS2 + 8].copy_from_slice(&(0x2 | DI_FLAGS2_BIGTIME).to_be_bytes());
        raw[FORKOFF] = 15;
        raw[AFORMAT] = 1; // local
        raw[ANEXTENTS..ANEXTENTS + 2].copy_from_slice(&3u16.to_be_bytes());

        let core = emptied_core(&raw);
        assert_eq!(
            u16::from_be_bytes(core[FLAGS..FLAGS + 2].try_into().unwrap()),
            0
        );
        assert_eq!(
            u64::from_be_bytes(core[FLAGS2..FLAGS2 + 8].try_into().unwrap()),
            DI_FLAGS2_BIGTIME
        );
        assert_eq!(core[FORKOFF], 0, "di_forkoff");
        assert_eq!(
            core[AFORMAT], 2,
            "di_aformat is EXTENTS, as an empty fork is"
        );
        assert_eq!(
            u16::from_be_bytes(core[ANEXTENTS..ANEXTENTS + 2].try_into().unwrap()),
            0
        );
        let _ = DI_FLAGS2_NREXT64;
    }

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

    /// A file a rename frees still held blocks when its core was read,
    /// and they go back in the same checkpoint. The freed core counts
    /// none and maps them in no format but an empty extent list, as
    /// `xfs_ifree` leaves it: the kernel's `xfs_dinode_verify` refuses
    /// an inode with no extents and blocks still counted, whatever its
    /// mode, so log recovery failed on the replaced file (#383).
    #[test]
    fn an_emptied_core_counts_no_blocks_and_maps_none() {
        let mut raw = vec![0u8; 176];
        raw[core_at::MODE..core_at::MODE + 2].copy_from_slice(&0o040755u16.to_be_bytes());
        raw[core_at::FORMAT] = 1; // local
        raw[core_at::NBLOCKS..core_at::NBLOCKS + 8].copy_from_slice(&16u64.to_be_bytes());
        raw[core_at::NEXTENTS..core_at::NEXTENTS + 4].copy_from_slice(&1u32.to_be_bytes());

        let core = emptied_core(&raw);
        assert_eq!(
            u64::from_be_bytes(
                core[core_at::NBLOCKS..core_at::NBLOCKS + 8]
                    .try_into()
                    .unwrap()
            ),
            0,
            "di_nblocks"
        );
        assert_eq!(core[core_at::FORMAT], 2, "di_format: extents");
        assert!(core[core_at::NEXTENTS..core_at::FORKOFF]
            .iter()
            .all(|&b| b == 0));
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
