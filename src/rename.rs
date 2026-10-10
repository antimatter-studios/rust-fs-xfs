//! Moving a name from one directory to another (#382).
//!
//! The name leaves its directory and arrives in the other in one record,
//! so a crash leaves it in one place or the other and never both or
//! neither. Each directory changes in whatever form it is in: a directory
//! in its inode edits its fork, and converts to block form if the name
//! does not fit; one past its inode is laid out again by
//! [`Filesystem::rewrite_directory`].
//!
//! A directory that moves takes its `..` with it. Its own `..` names the
//! new parent, the old parent loses the link that `..` was, and the new
//! parent gains it. A directory cannot be moved into itself or anything
//! beneath it: that would make a loop no path reaches, and is refused
//! before anything is written.
//!
//! Replacing a name that already exists in the target directory is #383,
//! and refused here.

use crate::create::clock_now;
use crate::dir;
use crate::dir_block::Entry;
use crate::error::{Error, Result};
use crate::format::log_items::inode_log_format::{XFS_ILOG_DDATA, XFS_ILOG_DEXT};
use crate::fs::Filesystem;
use crate::group_write::Allocations;
use crate::inode::{stamp_change, Changed, Format, Inode};
use crate::log_write::{
    inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer,
    Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that a directory's change moves.
mod at {
    pub const FORMAT: usize = 5;
    pub const NLINK: usize = 16;
    pub const SIZE: usize = 56;
    pub const NBLOCKS: usize = 64;
    pub const NEXTENTS: usize = 76;
    pub const NEXTENTS64: usize = 24;
    pub const CHANGECOUNT: usize = 104;
    pub const FLAGS2: usize = 120;
}

/// What changes in one directory.
enum Edit<'a> {
    Remove(&'a [u8]),
    Add(Entry),
    Reparent(u64),
}

/// A directory as one edit leaves it.
struct Changed_ {
    fork: Vec<u8>,
    flags: u32,
    format: Format,
    size: u64,
    blocks: u64,
    nextents: u64,
    items: Vec<crate::buf_write::BufferItem>,
}

impl Filesystem {
    /// Move `from` in directory `from_dir` to `to` in directory `to_dir`.
    ///
    /// Within one directory this is [`Filesystem::rename_in_directory`].
    /// Returns the sequence number the record was given.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotADirectory`] if either directory is not one,
    /// [`Error::NotFound`] if `from` is not there, [`Error::AlreadyExists`]
    /// if `to` is (replacing it is #383), and
    /// [`Error::UnsupportedFeature`] for a directory moved beneath itself
    /// and for the shapes either directory cannot be rewritten in. Every
    /// refusal comes before anything is written.
    pub fn rename(&self, from_dir: u64, from: &[u8], to_dir: u64, to: &[u8]) -> Result<u64> {
        if from_dir == to_dir {
            return self.rename_in_directory(from_dir, from, to);
        }
        self.writable_device()?;
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "renaming writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if !dir::entry_name_is_valid(to) || from == b"." || from == b".." {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} cannot be moved to {:?}",
                String::from_utf8_lossy(from),
                String::from_utf8_lossy(to)
            )));
        }

        let (src, src_raw) = self.read_inode_raw(from_dir)?;
        let (dst, dst_raw) = self.read_inode_raw(to_dir)?;
        if !src.is_dir() || !dst.is_dir() {
            return Err(Error::NotADirectory);
        }
        let moved_entry = self
            .read_dir(&src, &src_raw)?
            .into_iter()
            .find(|e| e.name == from)
            .ok_or(Error::NotFound)?;
        if self.read_dir(&dst, &dst_raw)?.iter().any(|e| e.name == to) {
            return Err(Error::AlreadyExists);
        }
        let moved_ino = moved_entry.ino;
        let (moved, moved_raw) = self.read_inode_raw(moved_ino)?;
        let is_dir = moved.is_dir();
        if is_dir {
            // Up from the target to the root: the moved directory must not
            // be on the way.
            let mut at = to_dir;
            let root = self.sb.rootino;
            let mut steps = 0;
            while at != root {
                if at == moved_ino {
                    return Err(Error::UnsupportedFeature(format!(
                        "directory inode {moved_ino} cannot be moved beneath itself"
                    )));
                }
                let (d, raw) = self.read_inode_raw(at)?;
                at = self.parent_of(&d, &raw)?;
                steps += 1;
                if steps > 1 << 16 {
                    return Err(Error::UnsupportedFeature(format!(
                        "the path up from inode {to_dir} does not reach the root"
                    )));
                }
            }
        }

        let mut allocations = Allocations::new();
        let src_change = self.change_directory(
            &mut allocations,
            from_dir,
            &src,
            &src_raw,
            Edit::Remove(from),
        )?;
        let dst_change = self.change_directory(
            &mut allocations,
            to_dir,
            &dst,
            &dst_raw,
            Edit::Add(Entry {
                name: to.to_vec(),
                ino: moved_ino,
                ftype: dir::ftype_to_raw(moved_entry.ftype),
            }),
        )?;
        let moved_change = if is_dir {
            Some(self.change_directory(
                &mut allocations,
                moved_ino,
                &moved,
                &moved_raw,
                Edit::Reparent(to_dir),
            )?)
        } else {
            None
        };
        let allocation_items = allocations.into_items()?;

        let mut quota_changes = Vec::new();
        for (inode, raw, change) in [(&src, &src_raw, &src_change), (&dst, &dst_raw, &dst_change)] {
            if change.blocks != inode.nblocks {
                quota_changes.push(crate::quota::QuotaChange {
                    uid: inode.uid,
                    gid: inode.gid,
                    project_id: crate::quota::project_id(raw),
                    blocks_fs: change.blocks as i64 - inode.nblocks as i64,
                    inodes: 0,
                });
            }
        }
        let quota_items = if quota_changes.is_empty() {
            Vec::new()
        } else {
            crate::quota::accounting_items(self, &quota_changes)?
        };

        // One clock reading for all three, as the kernel's `xfs_rename`.
        let when = clock_now();
        let link = |was: u32, delta: i32| -> Result<u32> {
            was.checked_add_signed(delta).ok_or_else(|| {
                Error::UnsupportedFeature(format!("a link count of {was} cannot move by {delta}"))
            })
        };
        let src_core = core_after(
            &src_raw,
            &src_change,
            is_dir.then(|| link(src.nlink, -1)).transpose()?,
            when,
            Changed::Contents,
        )?;
        let dst_core = core_after(
            &dst_raw,
            &dst_change,
            is_dir.then(|| link(dst.nlink, 1)).transpose()?,
            when,
            Changed::Contents,
        )?;
        let moved_core = match &moved_change {
            Some(change) => core_after(&moved_raw, change, None, when, Changed::Status)?,
            None => {
                let mut core = moved_raw.clone();
                bump(&mut core);
                stamp_change(&mut core, when, Changed::Status);
                core
            }
        };

        let cluster = self.sb.inode_cluster_bytes();
        let logged = |ino: u64, core: &[u8]| -> Result<(Vec<u8>, InodeBuffer)> {
            Ok((
                log_dinode_from_disk(core)
                    .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?,
                InodeBuffer::containing(self.inode_offset(ino)?, cluster),
            ))
        };
        let (src_logged, src_buf) = logged(from_dir, &src_core)?;
        let (dst_logged, dst_buf) = logged(to_dir, &dst_core)?;
        let (moved_logged, moved_buf) = logged(moved_ino, &moved_core)?;
        let padded = |fork: &[u8]| {
            let mut op = fork.to_vec();
            op.resize(fork.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
            op
        };

        let item_ops = allocation_items.iter().map(|i| i.op_count()).sum::<usize>()
            + src_change.items.iter().map(|i| i.op_count()).sum::<usize>()
            + dst_change.items.iter().map(|i| i.op_count()).sum::<usize>()
            + moved_change
                .as_ref()
                .map_or(0, |c| c.items.iter().map(|i| i.op_count()).sum::<usize>())
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + 3
            + 3
            + if moved_change.is_some() { 3 } else { 2 };

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
            for item in allocation_items
                .iter()
                .chain(&src_change.items)
                .chain(&dst_change.items)
                .chain(moved_change.iter().flat_map(|c| &c.items))
            {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            for (ino, change, buf, logged) in [
                (from_dir, &src_change, &src_buf, &src_logged),
                (to_dir, &dst_change, &dst_buf, &dst_logged),
            ] {
                ops.push(Op {
                    flags: 0,
                    data: inode_log_format_with_fork(
                        ino,
                        XFS_ILOG_CORE | change.flags,
                        buf,
                        change.fork.len() as u16,
                    ),
                });
                ops.push(Op {
                    flags: 0,
                    data: logged.clone(),
                });
                ops.push(Op {
                    flags: 0,
                    data: padded(&change.fork),
                });
            }
            match &moved_change {
                Some(change) => {
                    ops.push(Op {
                        flags: 0,
                        data: inode_log_format_with_fork(
                            moved_ino,
                            XFS_ILOG_CORE | change.flags,
                            &moved_buf,
                            change.fork.len() as u16,
                        ),
                    });
                    ops.push(Op {
                        flags: 0,
                        data: moved_logged.clone(),
                    });
                    ops.push(Op {
                        flags: 0,
                        data: padded(&change.fork),
                    });
                }
                None => {
                    ops.push(Op {
                        flags: 0,
                        data: inode_log_format(moved_ino, XFS_ILOG_CORE, &moved_buf),
                    });
                    ops.push(Op {
                        flags: 0,
                        data: moved_logged.clone(),
                    });
                }
            }
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&allocation_items);
        self.logged_buffers(&src_change.items);
        self.logged_buffers(&dst_change.items);
        if let Some(change) = &moved_change {
            self.logged_buffers(&change.items);
        }
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        self.logged_inode(from_dir, &src_core, &src_change.fork)?;
        self.logged_inode(to_dir, &dst_core, &dst_change.fork)?;
        match &moved_change {
            Some(change) => self.logged_inode(moved_ino, &moved_core, &change.fork)?,
            None => self.logged_inode(moved_ino, &moved_core, &[])?,
        }
        Ok(lsn)
    }

    /// The inode `..` names in directory `dir`.
    fn parent_of(&self, dir: &Inode, raw: &[u8]) -> Result<u64> {
        match dir.format {
            Format::Local => {
                let (start, end) = dir.data_fork_range(usize::from(self.sb.inodesize));
                Ok(dir::read_short_form(dir, &raw[start..end], &self.sb)?.parent_ino)
            }
            _ => Ok(self.entries_in_blocks(dir, raw)?[1].ino),
        }
    }

    /// Apply `edit` to directory `ino`, in whatever form it is in.
    fn change_directory<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        dir: &Inode,
        raw: &[u8],
        edit: Edit,
    ) -> Result<Changed_> {
        let (start, end) = dir.data_fork_range(usize::from(self.sb.inodesize));
        let space = end - start;
        let kept = |fork: Vec<u8>| Changed_ {
            size: fork.len() as u64,
            fork,
            flags: XFS_ILOG_DDATA,
            format: Format::Local,
            blocks: dir.nblocks,
            nextents: dir.nextents,
            items: Vec::new(),
        };
        let rewritten = |rw: crate::dir_edit::Rewritten| Changed_ {
            fork: rw.fork,
            flags: XFS_ILOG_DEXT,
            format: Format::Extents,
            size: rw.size,
            blocks: rw.blocks,
            nextents: rw.nextents,
            items: rw.items,
        };
        match dir.format {
            Format::Local => {
                let parsed = dir::read_short_form(dir, &raw[start..end], &self.sb)?;
                match edit {
                    Edit::Remove(name) => {
                        Ok(kept(self.short_form_without_entry(&parsed, name, space)?))
                    }
                    Edit::Reparent(parent) => {
                        Ok(kept(self.short_form_reparented(&parsed, parent, space)?))
                    }
                    Edit::Add(entry) => {
                        match self.short_form_with_entry(
                            &parsed,
                            &entry.name,
                            entry.ino,
                            entry.ftype,
                            space,
                        )? {
                            Some(fork) => Ok(kept(fork)),
                            // It does not fit: the directory leaves its inode.
                            None => {
                                let mut entries =
                                    crate::dir_block::entries_from_short_form(&parsed, ino, None);
                                entries.push(entry);
                                Ok(rewritten(self.rewrite_directory(
                                    allocations,
                                    ino,
                                    dir,
                                    raw,
                                    &entries,
                                )?))
                            }
                        }
                    }
                }
            }
            Format::Extents => {
                let mut entries = self.entries_in_blocks(dir, raw)?;
                match edit {
                    Edit::Remove(name) => entries.retain(|e| e.name != name),
                    Edit::Add(entry) => entries.push(entry),
                    Edit::Reparent(parent) => entries[1].ino = parent,
                }
                Ok(rewritten(self.rewrite_directory(
                    allocations,
                    ino,
                    dir,
                    raw,
                    &entries,
                )?))
            }
            other => Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps its entries in {other:?} form, which a move does not \
                 understand"
            ))),
        }
    }
}

/// Bump the change counter.
fn bump(core: &mut [u8]) {
    let now = u64::from_be_bytes(
        core[at::CHANGECOUNT..at::CHANGECOUNT + 8]
            .try_into()
            .expect("8 bytes"),
    );
    core[at::CHANGECOUNT..at::CHANGECOUNT + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
}

/// A directory's core after `change`, with its link count set to `nlink`
/// when that moved.
fn core_after(
    raw: &[u8],
    change: &Changed_,
    nlink: Option<u32>,
    when: crate::inode::Timestamp,
    changed: Changed,
) -> Result<Vec<u8>> {
    let mut core = raw.to_vec();
    core[at::FORMAT] = change.format as u8;
    core[at::SIZE..at::SIZE + 8].copy_from_slice(&change.size.to_be_bytes());
    core[at::NBLOCKS..at::NBLOCKS + 8].copy_from_slice(&change.blocks.to_be_bytes());
    let nrext64 = u64::from_be_bytes(raw[at::FLAGS2..at::FLAGS2 + 8].try_into().expect("8"))
        & crate::format::log_items::log_dinode::flags2::DI_FLAGS2_NREXT64
        != 0;
    if nrext64 {
        core[at::NEXTENTS64..at::NEXTENTS64 + 8].copy_from_slice(&change.nextents.to_be_bytes());
    } else {
        let n = u32::try_from(change.nextents)
            .map_err(|_| Error::UnsupportedFeature("an extent count past 32 bits".into()))?;
        core[at::NEXTENTS..at::NEXTENTS + 4].copy_from_slice(&n.to_be_bytes());
    }
    if let Some(n) = nlink {
        core[at::NLINK..at::NLINK + 4].copy_from_slice(&n.to_be_bytes());
    }
    bump(&mut core);
    stamp_change(&mut core, when, changed);
    Ok(core)
}
