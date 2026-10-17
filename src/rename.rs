//! Moving a name, within a directory or to another, and over a name that
//! is already there (#382, #383).
//!
//! Everything a rename changes goes in one record, so a crash leaves the
//! name where it was or where it went, never both or neither, and a
//! replaced target either still there or gone with everything it held.
//! Each directory changes in whatever form it is in: a directory in its
//! inode edits its fork, and converts to block form if a name does not
//! fit; one past its inode is laid out again by
//! [`Filesystem::rewrite_directory`].
//!
//! A directory that moves takes its `..` with it: its own `..` names the
//! new parent, the old parent loses the link that `..` was, and the new
//! parent gains it. A directory cannot be moved into itself or anything
//! beneath it, which would make a loop no path reaches.
//!
//! # Replacing a target (#383)
//!
//! The name may already exist where it is going. A file replaces a file;
//! a directory replaces an empty directory. The replaced inode loses the
//! link the name was, and an inode left with none is freed in the same
//! record: its slot goes back to its chunk, its extents to free space,
//! its attribute fork's blocks with them (#495), and its quota with all
//! of it. What POSIX forbids is refused before anything is
//! written: a file over a directory ([`Error::NotAFile`], EISDIR), a
//! directory over anything else ([`Error::NotADirectory`], ENOTDIR), a
//! directory over one that is not empty ([`Error::DirectoryNotEmpty`]).
//! Two names for the same inode is a rename that does nothing.

use crate::create::clock_now;
use crate::dir;
use crate::dir_block::Entry;
use crate::error::{Error, Result};
use crate::format::log_items::inode_log_format::XFS_ILOG_DDATA;
use crate::fs::Filesystem;
use crate::group_write::Allocations;
use crate::inode::{stamp_change, Changed, Format, Inode};
use crate::log_write::{
    inode_log_format_with_fork, log_dinode_from_disk, trans_header, InodeBuffer, Op, XFS_ILOG_CORE,
    XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; the fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// Offsets within the on-disk inode core that a rename moves.
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

use crate::dir_edit::DirEdit as Edit;

/// A directory as its edits leave it.
struct Changed_ {
    /// The fork as the inode item logs it.
    fork: Vec<u8>,
    /// The fork as the inode stores it, which a B+tree root is not.
    disk: Vec<u8>,
    flags: u32,
    format: Format,
    size: u64,
    blocks: u64,
    nextents: u64,
    items: Vec<crate::buf_write::BufferItem>,
}

/// One inode the record logs: its number, its new core, and its fork when
/// that changed too.
struct Logged {
    ino: u64,
    core: Vec<u8>,
    fork: Option<(u32, Vec<u8>, Vec<u8>)>,
    /// The attribute fork, when its parent pointers changed (#373); empty
    /// when it did not.
    afork: Vec<u8>,
    /// What `afork` is: `XFS_ILOG_ADATA` for short form, `XFS_ILOG_AEXT`
    /// for the extent record of a fork in blocks (#488).
    afields: u32,
}

impl Filesystem {
    /// Move `from` in directory `from_dir` to `to` in directory `to_dir`,
    /// replacing `to` if it is there.
    ///
    /// Returns the sequence number the record was given, or 0 when the two
    /// names are already the same inode and nothing changes.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotADirectory`] if either directory is not one or a
    /// directory would replace something else, [`Error::NotAFile`] if a
    /// file would replace a directory, [`Error::DirectoryNotEmpty`] if the
    /// directory replaced is not empty, [`Error::NotFound`] if `from` is
    /// not there, and [`Error::UnsupportedFeature`] for a directory moved
    /// beneath itself and for the shapes it cannot change. Every refusal
    /// comes before anything is written.
    pub fn rename(&self, from_dir: u64, from: &[u8], to_dir: u64, to: &[u8]) -> Result<u64> {
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
        let same_dir = from_dir == to_dir;

        let (src, src_raw) = self.read_inode_raw(from_dir)?;
        let (dst, dst_raw) = self.read_inode_raw(to_dir)?;
        if !src.is_dir() || !dst.is_dir() {
            return Err(Error::NotADirectory);
        }
        // Both names through the hash index, not a listing, which reads
        // every data block of a directory however big it is.
        let moved_ino = self
            .entry_ino(&src, &src_raw, from)?
            .ok_or(Error::NotFound)?;
        let target = self.entry_ino(&dst, &dst_raw, to)?;
        if same_dir && target.is_none() {
            return self.rename_in_directory(from_dir, from, to);
        }
        if target == Some(moved_ino) {
            return Ok(0);
        }
        let (moved, moved_raw) = self.read_inode_raw(moved_ino)?;
        let is_dir = moved.is_dir();

        // What the target is, and whether it may be replaced.
        let victim = match target {
            Some(t) => {
                let (inode, raw) = self.read_inode_raw(t)?;
                match (is_dir, inode.is_dir()) {
                    (false, true) => return Err(Error::NotAFile),
                    (true, false) => return Err(Error::NotADirectory),
                    (true, true) if !self.read_dir(&inode, &raw)?.is_empty() => {
                        return Err(Error::DirectoryNotEmpty)
                    }
                    _ => {}
                }
                Some((inode, raw))
            }
            None => None,
        };
        let victim_dir = victim.as_ref().is_some_and(|(v, _)| v.is_dir());
        let victim_freed = victim
            .as_ref()
            .is_some_and(|(v, _)| v.is_dir() || v.nlink <= 1);
        if let Some((v, _)) = victim
            .as_ref()
            .filter(|(v, _)| victim_freed && v.format == Format::Btree)
        {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} keeps its extents in a B+tree, which freeing it with a rename does \
                 not undo",
                v.ino
            )));
        }

        if is_dir && !same_dir {
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

        let arriving = Entry {
            name: to.to_vec(),
            ino: moved_ino,
            ftype: dir::ftype_to_raw(moved.file_type()),
        };
        let mut allocations = Allocations::new();
        let (src_change, dst_change) = if same_dir {
            let change = self.change_directory(
                &mut allocations,
                from_dir,
                &src,
                &src_raw,
                vec![Edit::Remove(from), Edit::Remove(to), Edit::Add(arriving)],
            )?;
            (change, None)
        } else {
            let mut dst_edits = Vec::new();
            if target.is_some() {
                dst_edits.push(Edit::Remove(to));
            }
            dst_edits.push(Edit::Add(arriving));
            (
                self.change_directory(
                    &mut allocations,
                    from_dir,
                    &src,
                    &src_raw,
                    vec![Edit::Remove(from)],
                )?,
                Some(self.change_directory(&mut allocations, to_dir, &dst, &dst_raw, dst_edits)?),
            )
        };
        let moved_change = if is_dir && !same_dir {
            Some(self.change_directory(
                &mut allocations,
                moved_ino,
                &moved,
                &moved_raw,
                vec![Edit::Reparent(to_dir)],
            )?)
        } else {
            None
        };

        // The replaced inode: a link fewer, or freed with what it held.
        let mut freed_items = Vec::new();
        let mut quota_changes = Vec::new();
        if let Some((v, raw)) = victim.as_ref().filter(|_| victim_freed) {
            freed_items = self.freed_inode_items(v.ino)?;
            let extents = match v.format {
                Format::Extents => self.data_extents(v, raw)?,
                _ => Vec::new(),
            };
            self.free_file_extents(&mut allocations, v.ino, &extents)?;
            quota_changes.push(crate::quota::QuotaChange {
                uid: v.uid,
                gid: v.gid,
                project_id: crate::quota::project_id(raw),
                blocks_fs: -(v.nblocks as i64),
                inodes: -1,
            });
        }
        let dirs: Vec<(&Inode, &Vec<u8>, &Changed_)> = match &dst_change {
            Some(d) => vec![(&src, &src_raw, &src_change), (&dst, &dst_raw, d)],
            None => vec![(&src, &src_raw, &src_change)],
        };
        for &(inode, raw, change) in &dirs {
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

        // Link counts. A directory leaving takes a link from its old parent
        // and gives one to its new; a directory replaced takes its `..`
        // from the directory it was in.
        let link = |was: u32, delta: i32| -> Result<u32> {
            was.checked_add_signed(delta).ok_or_else(|| {
                Error::UnsupportedFeature(format!("a link count of {was} cannot move by {delta}"))
            })
        };
        let src_delta = -i32::from(is_dir && !same_dir) - i32::from(same_dir && victim_dir);
        let dst_delta = i32::from(is_dir) - i32::from(victim_dir);

        // One clock reading for every inode, as the kernel's `xfs_rename`.
        let when = clock_now();
        let mut logged = Vec::new();
        logged.push(Logged {
            ino: from_dir,
            core: core_after(
                &src_raw,
                &src_change,
                (src_delta != 0)
                    .then(|| link(src.nlink, src_delta))
                    .transpose()?,
                when,
                Changed::Contents,
            )?,
            fork: Some((
                src_change.flags,
                src_change.fork.clone(),
                src_change.disk.clone(),
            )),
            afork: Vec::new(),
            afields: 0,
        });
        if let Some(d) = &dst_change {
            logged.push(Logged {
                ino: to_dir,
                core: core_after(
                    &dst_raw,
                    d,
                    (dst_delta != 0)
                        .then(|| link(dst.nlink, dst_delta))
                        .transpose()?,
                    when,
                    Changed::Contents,
                )?,
                fork: Some((d.flags, d.fork.clone(), d.disk.clone())),
                afork: Vec::new(),
                afields: 0,
            });
        }
        logged.push(match &moved_change {
            Some(change) => Logged {
                ino: moved_ino,
                core: core_after(&moved_raw, change, None, when, Changed::Status)?,
                fork: Some((change.flags, change.fork.clone(), change.disk.clone())),
                afork: Vec::new(),
                afields: 0,
            },
            None => {
                let mut core = moved_raw.clone();
                bump(&mut core);
                stamp_change(&mut core, when, Changed::Status);
                Logged {
                    ino: moved_ino,
                    core,
                    fork: None,
                    afork: Vec::new(),
                    afields: 0,
                }
            }
        });
        // THE PARENT POINTERS (#373): the moved inode trades its old name's
        // for its new one's, and a replaced inode that lives on loses its
        // name's. A freed one's attribute fork goes with it. Either fork
        // may be past short form and rebuilt in blocks (#488); both take
        // their new blocks before either gives its old ones back.
        let parents = self.sb.features_incompat & crate::superblock::incompat::PARENT != 0;
        let mut forks = Vec::new();
        if parents {
            let moved = logged.last_mut().expect("the moved inode is logged");
            let mut fork = self.with_parent_pointers(
                &mut allocations,
                moved_ino,
                &moved.core,
                &[
                    crate::attr_write::ParentChange::Remove {
                        name: from,
                        parent: from_dir,
                    },
                    crate::attr_write::ParentChange::Add {
                        name: to,
                        parent: to_dir,
                        generation: dst.gen,
                    },
                ],
            )?;
            moved.core = std::mem::take(&mut fork.core);
            moved.afork = fork.fork.clone();
            moved.afields = fork.fields;
            forks.push(fork);
        }
        if let Some((v, raw)) = &victim {
            let mut afork = Vec::new();
            let mut afields = 0;
            let core = if victim_freed {
                crate::unlink::emptied_core(raw)
            } else {
                let mut core = raw.clone();
                core[at::NLINK..at::NLINK + 4].copy_from_slice(&(v.nlink - 1).to_be_bytes());
                bump(&mut core);
                stamp_change(&mut core, when, Changed::Status);
                if parents {
                    let mut fork = self.with_parent_pointers(
                        &mut allocations,
                        v.ino,
                        &core,
                        &[crate::attr_write::ParentChange::Remove {
                            name: to,
                            parent: to_dir,
                        }],
                    )?;
                    core = std::mem::take(&mut fork.core);
                    afork = fork.fork.clone();
                    afields = fork.fields;
                    forks.push(fork);
                }
                core
            };
            logged.push(Logged {
                ino: v.ino,
                core,
                fork: None,
                afork,
                afields,
            });
        }
        for fork in &mut forks {
            fork.release(self, &mut allocations)?;
        }
        // A FREED VICTIM'S ATTRIBUTE BLOCKS (#495): its leaf, node and
        // remote-value blocks and the B+tree mapping them, given back after
        // every fork has taken its new blocks, so none is handed out again
        // in this record. Its quota credit, above, already counts them.
        let attr_cancels = match victim.as_ref().filter(|_| victim_freed) {
            Some((v, raw)) => self.release_freed_attribute_fork(&mut allocations, v, raw)?,
            None => Vec::new(),
        };
        quota_changes.extend(forks.iter().filter_map(|f| f.quota));
        let allocation_items = allocations.into_items()?;
        let quota_items = if quota_changes.is_empty() {
            Vec::new()
        } else {
            crate::quota::accounting_items(self, &quota_changes)?
        };

        let cluster = self.sb.inode_cluster_bytes();
        let mut inode_ops = Vec::new();
        for l in &logged {
            let buf = InodeBuffer::containing(self.inode_offset(l.ino)?, cluster);
            let core = log_dinode_from_disk(&l.core)
                .map_err(|why| Error::UnsupportedFeature(format!("inode {}: {why}", l.ino)))?;
            match &l.fork {
                Some((flags, fork, _)) if !l.afork.is_empty() => {
                    // Data fork and attribute fork both (#373).
                    let mut op = fork.clone();
                    op.resize(fork.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
                    let mut attr = l.afork.clone();
                    attr.resize(l.afork.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
                    inode_ops.push(crate::log_write::inode_log_format_with_forks(
                        l.ino,
                        XFS_ILOG_CORE | flags | l.afields,
                        &buf,
                        fork.len() as u16,
                        l.afork.len() as u16,
                    ));
                    inode_ops.push(core);
                    inode_ops.push(op);
                    inode_ops.push(attr);
                }
                Some((flags, fork, _)) => {
                    let mut op = fork.clone();
                    op.resize(fork.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
                    inode_ops.push(inode_log_format_with_fork(
                        l.ino,
                        XFS_ILOG_CORE | flags,
                        &buf,
                        fork.len() as u16,
                    ));
                    inode_ops.push(core);
                    inode_ops.push(op);
                }
                None => {
                    inode_ops.extend(
                        crate::log_write::inode_ops_with_attr(
                            l.ino, &buf, core, &l.afork, l.afields,
                        )
                        .into_iter()
                        .map(|op| op.data),
                    );
                }
            }
        }
        let dir_items: Vec<&crate::buf_write::BufferItem> = src_change
            .items
            .iter()
            .chain(dst_change.iter().flat_map(|c| &c.items))
            .chain(moved_change.iter().flat_map(|c| &c.items))
            .collect();
        let item_ops = allocation_items.iter().map(|i| i.op_count()).sum::<usize>()
            + freed_items.iter().map(|i| i.op_count()).sum::<usize>()
            + dir_items.iter().map(|i| i.op_count()).sum::<usize>()
            + forks.iter().map(|f| f.buffer_op_count()).sum::<usize>()
            + attr_cancels.iter().map(|i| i.op_count()).sum::<usize>()
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + inode_ops.len();

        // A remote value in a rebuilt attribute fork is on the device
        // before the record that maps it (#501).
        for fork in &forks {
            fork.write_remote(self)?;
        }
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
            for item in allocation_items.iter().chain(&freed_items) {
                ops.extend(item.ops());
            }
            for item in &dir_items {
                ops.extend(item.ops());
            }
            for fork in &forks {
                ops.extend(fork.buffer_ops());
            }
            for item in &attr_cancels {
                ops.extend(item.ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            ops.extend(inode_ops.iter().cloned().map(|data| Op { flags: 0, data }));
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        // What the record says is now what this mount reads (#89). The
        // freed attribute blocks first, so nothing this record logs is let
        // go of with them.
        self.cancelled_buffers(&attr_cancels);
        self.logged_buffers(&allocation_items);
        self.logged_buffers(&freed_items);
        for item in dir_items {
            self.logged_buffers(std::slice::from_ref(item));
        }
        for fork in &forks {
            self.logged_buffers(fork.buffers());
        }
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        for l in &logged {
            let fork = l.fork.as_ref().map(|(_, _, d)| d.as_slice()).unwrap_or(&[]);
            self.logged_inode(l.ino, &l.core, fork)?;
        }
        Ok(lsn)
    }

    /// Give inode `ino` another name: `name` in directory `dir_ino` (#384).
    ///
    /// The directory gains the entry, in whatever form it is in, and the
    /// inode's link count rises by one, in one record. Returns the
    /// sequence number the record was given.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotADirectory`] if `dir_ino` is not one,
    /// [`Error::AlreadyExists`] if `name` is taken, and
    /// [`Error::UnsupportedFeature`] for a directory, which cannot have a
    /// second name, a link count already at its limit, and the shapes the
    /// directory cannot be rewritten in. Every refusal comes before
    /// anything is written.
    pub fn link(&self, ino: u64, dir_ino: u64, name: &[u8]) -> Result<u64> {
        self.writable_device()?;
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "linking writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if !dir::entry_name_is_valid(name) {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} is not a name a directory entry can hold",
                String::from_utf8_lossy(name)
            )));
        }
        let (dir, dir_raw) = self.read_inode_raw(dir_ino)?;
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        let (inode, raw) = self.read_inode_raw(ino)?;
        if inode.is_dir() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} is a directory, which has one name and cannot be given another"
            )));
        }
        if inode.nlink == 0 {
            return Err(Error::NotFound);
        }
        // XFS_MAXLINK: the most names one inode may have.
        if inode.nlink >= (1 << 31) - 1 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} already has {} links, the most it may have",
                inode.nlink
            )));
        }
        // Through the hash index, not a listing: a listing reads every
        // data block, so each link into a node-form directory cost as
        // much as the directory was big.
        if self.entry_ino(&dir, &dir_raw, name)?.is_some() {
            return Err(Error::AlreadyExists);
        }

        let mut allocations = Allocations::new();
        let change = self.change_directory(
            &mut allocations,
            dir_ino,
            &dir,
            &dir_raw,
            vec![Edit::Add(Entry {
                name: name.to_vec(),
                ino,
                ftype: crate::dir::ftype_to_raw(inode.file_type()),
            })],
        )?;
        let when = clock_now();
        let dir_core = core_after(&dir_raw, &change, None, when, Changed::Contents)?;
        let mut inode_core = raw.clone();
        inode_core[at::NLINK..at::NLINK + 4].copy_from_slice(&(inode.nlink + 1).to_be_bytes());
        bump(&mut inode_core);
        stamp_change(&mut inode_core, when, Changed::Status);
        // The new name's parent pointer, in the same record (#373), in
        // whatever form the attribute fork is in (#488).
        let mut parents = None;
        if self.sb.features_incompat & crate::superblock::incompat::PARENT != 0 {
            let mut fork = self.with_parent_pointers(
                &mut allocations,
                ino,
                &inode_core,
                &[crate::attr_write::ParentChange::Add {
                    name,
                    parent: dir_ino,
                    generation: dir.gen,
                }],
            )?;
            fork.release(self, &mut allocations)?;
            inode_core = std::mem::take(&mut fork.core);
            parents = Some(fork);
        }
        let allocation_items = allocations.into_items()?;
        let mut quota_changes = Vec::new();
        if change.blocks != dir.nblocks {
            quota_changes.push(crate::quota::QuotaChange {
                uid: dir.uid,
                gid: dir.gid,
                project_id: crate::quota::project_id(&dir_raw),
                blocks_fs: change.blocks as i64 - dir.nblocks as i64,
                inodes: 0,
            });
        }
        quota_changes.extend(parents.as_ref().and_then(|p| p.quota));
        let quota_items = if quota_changes.is_empty() {
            Vec::new()
        } else {
            crate::quota::accounting_items(self, &quota_changes)?
        };

        let cluster = self.sb.inode_cluster_bytes();
        let dir_buf = InodeBuffer::containing(self.inode_offset(dir_ino)?, cluster);
        let inode_buf = InodeBuffer::containing(self.inode_offset(ino)?, cluster);
        let dir_logged = log_dinode_from_disk(&dir_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {dir_ino}: {why}")))?;
        let inode_logged = log_dinode_from_disk(&inode_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;
        let mut fork_op = change.fork.clone();
        fork_op.resize(change.fork.len().div_ceil(OP_ALIGN) * OP_ALIGN, 0);
        let (afork, afields) = parents
            .as_ref()
            .map_or((&[][..], 0), |p| (p.fork.as_slice(), p.fields));
        let inode_ops =
            crate::log_write::inode_ops_with_attr(ino, &inode_buf, inode_logged, afork, afields);
        let item_ops = allocation_items.iter().map(|i| i.op_count()).sum::<usize>()
            + change.items.iter().map(|i| i.op_count()).sum::<usize>()
            + parents.as_ref().map_or(0, |p| p.buffer_op_count())
            + quota_items.iter().map(|i| i.op_count()).sum::<usize>()
            + 3
            + inode_ops.len();
        // A remote value in a rebuilt attribute fork is on the device
        // before the record that maps it (#501).
        if let Some(p) = &parents {
            p.write_remote(self)?;
        }
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
            for item in allocation_items.iter().chain(&change.items) {
                ops.extend(item.ops());
            }
            if let Some(p) = &parents {
                ops.extend(p.buffer_ops());
            }
            for item in &quota_items {
                ops.extend(item.ops());
            }
            ops.extend([
                Op {
                    flags: 0,
                    data: inode_log_format_with_fork(
                        dir_ino,
                        XFS_ILOG_CORE | change.flags,
                        &dir_buf,
                        change.fork.len() as u16,
                    ),
                },
                Op {
                    flags: 0,
                    data: dir_logged,
                },
                Op {
                    flags: 0,
                    data: fork_op,
                },
            ]);
            ops.extend(inode_ops.clone());
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        // What the record says is now what this mount reads (#89).
        self.logged_buffers(&allocation_items);
        self.logged_buffers(&change.items);
        if let Some(p) = &parents {
            self.logged_buffers(p.buffers());
        }
        for item in &quota_items {
            item.apply_overlay(self)?;
        }
        self.logged_inode(dir_ino, &dir_core, &change.disk)?;
        self.logged_inode(ino, &inode_core, &[])?;
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

    /// Apply `edits` to directory `ino`, in whatever form it is in.
    fn change_directory<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        dir: &Inode,
        raw: &[u8],
        edits: Vec<Edit>,
    ) -> Result<Changed_> {
        let (start, end) = dir.data_fork_range(usize::from(self.sb.inodesize));
        let space = end - start;
        let rewritten = |rw: crate::dir_edit::Rewritten| Changed_ {
            fork: rw.logged_fork,
            disk: rw.fork,
            flags: rw.fields,
            format: rw.format,
            size: rw.size,
            blocks: rw.blocks,
            nextents: rw.nextents,
            items: rw.items,
        };
        if matches!(dir.format, Format::Extents | Format::Btree) {
            return Ok(rewritten(self.edit_directory(
                allocations,
                ino,
                dir,
                raw,
                &edits,
            )?));
        }
        if dir.format != Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} keeps its entries in {:?} form, which a rename does not \
                 understand",
                dir.format
            )));
        }
        let parsed = dir::read_short_form(dir, &raw[start..end], &self.sb)?;
        let mut entries = crate::dir_block::entries_from_short_form(&parsed, ino, None);
        for edit in edits {
            match edit {
                Edit::Remove(name) => entries.retain(|e| e.name != name),
                Edit::Add(entry) => entries.push(entry),
                Edit::Reparent(parent) => entries[1].ino = parent,
                Edit::Rename(from, to) => {
                    for e in entries.iter_mut().skip(2).filter(|e| e.name == from) {
                        e.name = to.to_vec();
                    }
                }
            }
        }
        // Short form keeps `..` in its header and neither dot as an entry.
        if let Some(fork) = self.short_form_of(&parsed, entries[1].ino, &entries[2..], space)? {
            return Ok(Changed_ {
                size: fork.len() as u64,
                disk: fork.clone(),
                fork,
                flags: XFS_ILOG_DDATA,
                format: Format::Local,
                blocks: dir.nblocks,
                nextents: dir.nextents,
                items: Vec::new(),
            });
        }
        Ok(rewritten(self.rewrite_directory(
            allocations,
            ino,
            dir,
            raw,
            &entries,
        )?))
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
