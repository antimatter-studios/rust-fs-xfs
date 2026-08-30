//! Creating a file, through the log.
//!
//! The first transaction with **five** items in it, and the first that
//! touches two inodes and three metadata blocks at once. Creating a file
//! means taking an inode out of an allocation group's accounting, making
//! that inode into a file, and adding a name for it to a directory — and
//! none of the three is any use without the other two.
//!
//! ```text
//! op  what
//!  0   START
//!  1   transaction header
//!  2   the group's inode header, and its dirty chunk
//!  ..  the inode tree's root
//!  ..  the free-inode tree's root
//!  ..  the parent directory's format, core and entries
//!  ..  the new inode's format and core
//!  ..  COMMIT
//! ```
//!
//! Fourteen operations for the smallest case, which is what a create was
//! measured to produce.
//!
//! # A free inode is not an empty one
//!
//! XFS initialises a whole chunk of inodes when it allocates the chunk,
//! not when it hands one out. So the inode a create takes already has
//! its magic, its version, its own inode number and the filesystem UUID
//! on disk, all correct — and this reads it and changes what a file
//! needs changed, rather than building a core from nothing.
//!
//! That is not a shortcut. `di_ino` and `di_uuid` are the fields a v5
//! filesystem uses to catch a block that landed in the wrong place, and
//! a core built from scratch would have to reproduce them exactly to be
//! accepted. Reading what is already there cannot get them wrong.
//!
//! # What it will not do
//!
//! Each is refused by name rather than attempted:
//!
//! - a group with no free inode, which needs a whole new chunk and so
//!   needs to allocate blocks as well;
//! - a parent that is not a short-form directory. A short-form parent
//!   with no room left is NOT refused: it is converted to block form
//!   (see `convert_to_block_form`), which is a feature of this module
//!   rather than a limit of it;
//! - a name that is already in the directory;
//! - inode trees more than one level deep. A root with no room is not
//!   checked here — the capacity refusal lives in `unlink`, and this
//!   list previously promised a guard `create` does not have;
//! - a v4 filesystem.
//!
//! # What is deliberately left alone
//!
//! **The timestamps.** There is no clock here, and the driver would have
//! to invent one. A created file gets whatever the free inode carried,
//! which is the epoch. That is visibly wrong rather than subtly wrong,
//! which is the better failure of the two, and it is what
//! [`crate::dir_write`] already does for the same reason.

use crate::ag::{offsets::agi as agi_at, Agi};
use crate::alloc_btree::expected_blkno;
use crate::dir;
use crate::dir_block;
use crate::error::{Error, Result};
use crate::format::log_items::buf_log_format::buf_type::{BLFT_AGI, BLFT_BTREE};
use crate::format::log_items::inode_log_format::{XFS_ILOG_DDATA, XFS_ILOG_DEXT};
use crate::fs::Filesystem;
use crate::group_write::{changed_chunks, rebuild_inode_leaf};
use crate::inode::Format;
use crate::inode_btree::{choose_free_inode, walk_from_agi, InodeChunk, Taken, Which};
use crate::log::BBSIZE;
use crate::log_write::{
    append, inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header,
    InodeBuffer, Op, XFS_ILOG_CORE, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// An operation's payload is padded to four bytes; a fork's own length
/// is not.
const OP_ALIGN: usize = 4;

/// What is being created, and everything that differs between the two.
///
/// A directory is a file with a fork, a different link count and a
/// parent whose own link count moves — and nothing else about the
/// transaction changes, which is why the two share one path rather than
/// two that would drift apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An empty regular file: no fork, one link, no size.
    File,
    /// An empty directory: a short-form fork holding only its parent,
    /// and **two** links — its own `.` and the entry naming it.
    Directory,
}

impl Kind {
    /// The fork format the new inode records.
    fn format(self) -> Format {
        match self {
            // An empty file keeps its extents inline, of which it has
            // none.
            Kind::File => Format::Extents,
            // A directory small enough lives inside its inode, and a
            // new one always is.
            Kind::Directory => Format::Local,
        }
    }

    /// The new inode's link count.
    ///
    /// A directory starts with two: the entry naming it in its parent,
    /// and its own `.`. Getting this wrong is not caught by reading the
    /// directory back — only by a consistency check, or by the
    /// directory refusing to be removed later.
    fn nlink(self) -> u32 {
        match self {
            Kind::File => 1,
            Kind::Directory => 2,
        }
    }

    /// What the parent's link count becomes, given what it was.
    ///
    /// A new subdirectory's `..` is a link to the parent, so the parent
    /// gains one. A file has no `..` and the parent does not move.
    fn parent_nlink(self, was: u32) -> u32 {
        match self {
            Kind::File => was,
            Kind::Directory => was + 1,
        }
    }

    /// The file type recorded in the directory entry.
    fn ftype(self) -> u8 {
        dir::ftype_to_raw(Some(match self {
            Kind::File => crate::inode::FileType::Regular,
            Kind::Directory => crate::inode::FileType::Directory,
        }))
    }
}

/// Offsets within the on-disk inode core that a create sets.
mod core_at {
    pub const MODE: usize = 2;
    pub const FORMAT: usize = 5;
    pub const NLINK: usize = 16;
    pub const NBLOCKS: usize = 64;
    pub const SIZE: usize = 56;
    pub const GEN: usize = 92;
    pub const CHANGECOUNT: usize = 104;
}

/// The inode core of a newly created file or directory.
///
/// Read from the free inode rather than built, so the identity fields
/// that are already correct stay correct.
fn created_core(raw: &[u8], mode: u16, kind: Kind, size: u64) -> Vec<u8> {
    let mut core = raw.to_vec();
    core[core_at::MODE..core_at::MODE + 2].copy_from_slice(&mode.to_be_bytes());
    core[core_at::FORMAT] = kind.format() as u8;
    core[core_at::NLINK..core_at::NLINK + 4].copy_from_slice(&kind.nlink().to_be_bytes());
    core[core_at::SIZE..core_at::SIZE + 8].copy_from_slice(&size.to_be_bytes());

    // The generation is what stops a stale reference to the inode's
    // previous life from resolving to its new one. Advancing it is the
    // whole of that protection, so it is advanced even though nothing
    // here would notice if it were not.
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

/// What converting a directory to block form produced.
struct Converted {
    /// The parent's new data fork: a single extent record naming the
    /// block the directory now lives in.
    fork: Vec<u8>,
    /// Its new size, which is one directory block.
    size: u64,
    /// The three items recording the allocation, then the directory
    /// block itself.
    items: Vec<crate::buf_write::BufferItem>,
}

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

impl Filesystem {
    /// Move a short-form directory into a block of its own, with `new`
    /// added.
    ///
    /// This is what happens when one more entry will not fit inside the
    /// inode. A block is allocated, the whole directory is written into
    /// it — `.`, `..`, every existing name and the new one, plus a hash
    /// index — and the inode's fork becomes a single extent naming it.
    ///
    /// # The block is not written to disk
    ///
    /// It goes into the record as a buffer item, like every other
    /// metadata change here, and recovery writes it. That is what keeps
    /// the operation checkable: a directory that came out converted is
    /// one something replayed.
    ///
    /// That differs from an allocating file write, which does put the
    /// file's bytes on disk before logging — because file data is not
    /// journalled and directory blocks are.
    ///
    /// # Errors
    ///
    /// As [`crate::group_write::Allocated`], and
    /// [`Error::UnsupportedFeature`] if the entries do not fit in one
    /// block — that is the leaf form.
    fn convert_to_block_form(
        &self,
        parent: u64,
        parsed: &dir::ShortFormDir,
        new: dir_block::Entry,
    ) -> Result<Converted> {
        use crate::extent::Extent;
        use crate::format::log_items::buf_log_format::buf_type::BLFT_DIR_BLOCK;
        use crate::group_write::changed_chunks;

        let dirblocksize = (u64::from(self.sb.blocksize) << self.sb.dirblklog) as usize;
        if dirblocksize != self.sb.blocksize as usize {
            return Err(Error::UnsupportedFeature(format!(
                "a directory block of {dirblocksize} bytes spans more than one filesystem                  block, so converting would allocate several; only a directory block the                  size of a filesystem block is supported"
            )));
        }

        // The block comes from the directory's own group, which keeps it
        // near the inode that names it.
        let (agno, _, _) = self.sb.split_ino(parent);
        let allocated = self.allocate_in_group(agno, 1)?;
        let fsblock = (u64::from(agno) << self.sb.agblklog) | u64::from(allocated.agblock);

        let entries = {
            let mut e = dir_block::entries_from_short_form(parsed, parent, None);
            e.push(new);
            e
        };
        let block = dir_block::build(&self.sb, fsblock, parent, &entries)?;

        // The block did not exist a moment ago, so every byte of it is a
        // change — but only the bytes that are not zero are worth
        // logging, and diffing against a block of zeros is what says
        // which those are. It comes to the header and entries at the
        // front and the index and tail at the back, with the free middle
        // left out, which is what the kernel logs too.
        let fresh = vec![0u8; dirblocksize];
        let blkno = fsblock * u64::from(self.sb.blocksize) / crate::log::BBSIZE as u64;
        let block_item = changed_chunks(blkno, &fresh, block, BLFT_DIR_BLOCK);

        let extent = Extent {
            startoff: 0,
            startblock: fsblock,
            blockcount: 1,
            unwritten: false,
        };

        let mut items = allocated.items;
        items.push(block_item);
        Ok(Converted {
            fork: extent.to_bytes()?.to_vec(),
            size: dirblocksize as u64,
            items,
        })
    }

    /// Create an empty regular file called `name` in `parent`.
    ///
    /// Returns the new file's inode number and the sequence number the
    /// record was given. Nothing on disk is touched: the record is the
    /// change.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::AlreadyExists`] if the name is taken,
    /// [`Error::NotADirectory`] if `parent` is not one, and
    /// [`Error::UnsupportedFeature`] for each of the shapes listed in
    /// this module's documentation.
    pub fn create_file(&self, parent: u64, name: &[u8], mode: u16) -> Result<(u64, u64)> {
        self.create(parent, name, mode, Kind::File)
    }

    /// Create an empty directory called `name` in `parent`.
    ///
    /// The new directory holds no entries: `.` and `..` are not entries
    /// in the short form, which keeps its parent in the header and its
    /// own identity in the inode. So its fork is the six-byte header
    /// alone, and its link count is two — the entry naming it, and its
    /// own `.`.
    ///
    /// The parent's link count goes up by one, because the new
    /// directory's `..` is a link to it. That is the whole difference
    /// between this and [`Filesystem::create_file`], and it is the part
    /// nothing catches by reading the directory back: a wrong link count
    /// shows up only in a consistency check, or later, when the
    /// directory refuses to be removed.
    ///
    /// Returns the new directory's inode number and the sequence number
    /// the record was given.
    ///
    /// # Errors
    ///
    /// As [`Filesystem::create_file`].
    pub fn create_directory(&self, parent: u64, name: &[u8], mode: u16) -> Result<(u64, u64)> {
        self.create(parent, name, mode, Kind::Directory)
    }

    fn create(&self, parent: u64, name: &[u8], mode: u16, kind: Kind) -> Result<(u64, u64)> {
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if !self.sb.is_v5() {
            return Err(Error::UnsupportedFeature(
                "creating writes v5 metadata; a v4 filesystem is not supported".into(),
            ));
        }
        if name.is_empty() || name.contains(&b'/') || name == b"." || name == b".." {
            return Err(Error::UnsupportedFeature(format!(
                "{:?} is not a name a directory entry can hold",
                String::from_utf8_lossy(name)
            )));
        }

        let (dir_inode, dir_raw) = self.read_inode_raw(parent)?;
        if !dir_inode.is_dir() {
            return Err(Error::NotADirectory);
        }
        if dir_inode.format != Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {parent} has outgrown the inode, so adding an entry rewrites a \
                 directory block rather than the inode's own fork"
            )));
        }

        let (fork_start, fork_end) = dir_inode.data_fork_range(usize::from(self.sb.inodesize));
        let parsed = dir::read_short_form(&dir_inode, &dir_raw[fork_start..fork_end], &self.sb)?;
        if parsed.entries.iter().any(|e| e.name == name) {
            return Err(Error::AlreadyExists);
        }

        // The new inode comes from the parent's own group, which is what
        // keeps a directory's files near the directory.
        let (agno, _, _) = self.sb.split_ino(parent);
        let block = u64::from(self.sb.blocksize);
        let ag_start = u64::from(agno) * u64::from(self.sb.agblocks) * block;
        let sector = u64::from(self.sb.sectsize);

        let mut agi_raw = vec![0u8; self.sb.sectsize as usize];
        self.device().read_at(ag_start + 2 * sector, &mut agi_raw)?;
        let agi = Agi::parse(&agi_raw, &self.sb, agno)?;

        for (level, what) in [(agi.level, "inode"), (agi.free_level, "free-inode")] {
            if level != 1 {
                return Err(Error::UnsupportedFeature(format!(
                    "allocation group {agno}'s {what} tree is {level} levels deep, where \
                     changing a record can reshape a node; only a single-level tree is \
                     supported"
                )));
            }
        }

        let read = |agblock: u32| -> Result<Vec<u8>> {
            let mut buf = vec![0u8; self.sb.blocksize as usize];
            self.device()
                .read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
            Ok(buf)
        };
        let mut chunks = walk_from_agi(&self.sb, &agi, Which::All, read)?
            .expect("every filesystem has an inode tree");

        let Some((index, slot)) = choose_free_inode(&chunks) else {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno} has no free inode; allocating a whole new chunk \
                 also allocates blocks, which is not implemented"
            )));
        };
        let agino = chunks[index].startino + u32::from(slot);
        let outcome = chunks[index].take(slot)?;

        // The new inode's absolute number, which is what a directory
        // entry names and what the inode itself records.
        let ino = self.sb.join_ino(agno, agino);

        let mut inobt_raw = read(agi.root)?;
        let mut finobt_raw = read(agi.free_root)?;
        let sparse = self.sb.has_sparse_inodes();

        let new_inobt = rebuild_inode_leaf(&inobt_raw, &chunks, sparse);

        // The free-inode tree holds only the chunks with something free,
        // so a chunk that has just been filled leaves it.
        let with_free: Vec<InodeChunk> =
            chunks.iter().copied().filter(|c| c.freecount > 0).collect();
        let new_finobt = rebuild_inode_leaf(&finobt_raw, &with_free, sparse);
        debug_assert_eq!(
            outcome == Taken::ChunkNowFull,
            !with_free
                .iter()
                .any(|c| c.startino == chunks[index].startino),
            "a chunk that is now full must have left the free-inode tree"
        );

        let mut new_agi = agi_raw.clone();
        let freecount: u32 = chunks.iter().map(|c| u32::from(c.freecount)).sum();
        new_agi[agi_at::FREECOUNT..agi_at::FREECOUNT + 4].copy_from_slice(&freecount.to_be_bytes());
        // The checksum is left stale on purpose — recovery recomputes it,
        // and writing it here would dirty a chunk nothing else touches and
        // add an operation to the record. See `group_write::restamp_crc`.

        // The parent gains an entry, so its fork and its size change —
        // unless the entry will not fit, in which case the directory
        // leaves its inode entirely and this becomes a conversion.
        let fork_space = fork_end - fork_start;
        let short_form =
            self.short_form_with_entry(&parsed, name, ino, kind.ftype(), fork_space)?;

        let converted = match &short_form {
            Some(_) => None,
            None => Some(self.convert_to_block_form(
                parent,
                &parsed,
                dir_block::Entry {
                    name: name.to_vec(),
                    ino,
                    ftype: kind.ftype(),
                },
            )?),
        };

        // Whichever it is, the parent's fork is these bytes and its size
        // is their length — a short-form directory's size is its fork,
        // and a converted one's is the block it now occupies.
        let (fork, dir_fields, dir_size, dir_blocks, dir_nextents, dir_format) =
            match (&short_form, &converted) {
                (Some(fork), _) => (
                    fork.clone(),
                    XFS_ILOG_DDATA,
                    fork.len() as u64,
                    dir_inode.nblocks,
                    dir_inode.nextents,
                    Format::Local,
                ),
                (None, Some(c)) => (c.fork.clone(), XFS_ILOG_DEXT, c.size, 1, 1, Format::Extents),
                (None, None) => unreachable!("one of the two is always taken"),
            };

        let mut dir_core = dir_raw.clone();
        dir_core[core_at::SIZE..core_at::SIZE + 8].copy_from_slice(&dir_size.to_be_bytes());
        dir_core[core_at::FORMAT] = dir_format as u8;
        dir_core[core_at::NBLOCKS..core_at::NBLOCKS + 8].copy_from_slice(&dir_blocks.to_be_bytes());
        set_nextents(&mut dir_core, dir_nextents);
        dir_core[core_at::NLINK..core_at::NLINK + 4]
            .copy_from_slice(&kind.parent_nlink(dir_inode.nlink).to_be_bytes());
        let at = core_at::CHANGECOUNT;
        let now = u64::from_be_bytes(dir_core[at..at + 8].try_into().expect("8 bytes"));
        dir_core[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());

        // The new inode, read rather than built — see the note at the
        // top on why the identity fields make that the safer of the two.
        // Read straight off the device rather than through
        // `read_inode_raw`: that validates, and a free inode has mode
        // zero and no fork, which is not a shape a file's parser should
        // have to accept.
        let mut new_raw = vec![0u8; usize::from(self.sb.inodesize)];
        self.device()
            .read_at(self.inode_offset(ino)?, &mut new_raw)?;
        // A new directory's fork is the short-form header alone: no
        // entries, and the parent it belongs to. `.` and `..` are not
        // entries in this form — the parent lives in the header and the
        // directory's own identity in the inode — so an empty directory
        // really is empty.
        let new_fork = match kind {
            Kind::File => Vec::new(),
            Kind::Directory => empty_short_form_dir(parent),
        };
        let new_core = created_core(&new_raw, mode, kind, new_fork.len() as u64);

        let dir_logged = log_dinode_from_disk(&dir_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {parent}: {why}")))?;
        let new_logged = log_dinode_from_disk(&new_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {ino}: {why}")))?;

        let cluster = self.sb.inode_cluster_bytes();
        let dir_buf = InodeBuffer::containing(self.inode_offset(parent)?, cluster);
        let new_buf = InodeBuffer::containing(self.inode_offset(ino)?, cluster);

        let dsize = fork.len();
        let mut fork_op = fork;
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);

        let ag_bb = ag_start / BBSIZE as u64;
        let agi_item = changed_chunks(
            ag_bb + 2 * sector / BBSIZE as u64,
            &agi_raw,
            new_agi,
            BLFT_AGI,
        );
        let inobt_item = changed_chunks(
            expected_blkno(&self.sb, agno, agi.root),
            &inobt_raw,
            new_inobt,
            BLFT_BTREE,
        );
        let finobt_item = changed_chunks(
            expected_blkno(&self.sb, agno, agi.free_root),
            &finobt_raw,
            new_finobt,
            BLFT_BTREE,
        );
        inobt_raw.clear();
        finobt_raw.clear();

        // Three operations for the parent — format, core and entries —
        // and two for the new inode, which logs no fork of its own.
        // The new inode's own fork operation, when it has one.
        let new_dsize = new_fork.len();
        let mut new_fork_op = new_fork;
        new_fork_op.resize(new_dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);

        // Three operations for the parent — format, core and entries —
        // and two or three for the new inode, depending on whether it
        // has a fork. That one operation is the whole difference between
        // a create's fourteen and a mkdir's fifteen.
        let new_ops = if new_dsize == 0 { 2 } else { 3 };

        // The conversion's own items, when there was one: the group
        // header, the two free-space trees and the directory block.
        // Empty otherwise, which is why the ordinary create's shape is
        // unchanged by any of this.
        let extra: Vec<crate::buf_write::BufferItem> = converted.map_or_else(Vec::new, |c| c.items);

        let item_ops = agi_item.op_count()
            + inobt_item.op_count()
            + finobt_item.op_count()
            + extra.iter().map(|i| i.op_count()).sum::<usize>()
            + 3
            + new_ops;

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
            ops.extend(agi_item.ops());
            ops.extend(inobt_item.ops());
            ops.extend(finobt_item.ops());
            for item in &extra {
                ops.extend(item.ops());
            }
            ops.push(Op {
                flags: 0,
                data: inode_log_format_with_fork(
                    parent,
                    // DDATA while the directory is still inline, DEXT
                    // once it has been moved into a block of its own.
                    XFS_ILOG_CORE | dir_fields,
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
            if new_dsize == 0 {
                ops.push(Op {
                    flags: 0,
                    data: inode_log_format(ino, XFS_ILOG_CORE, &new_buf),
                });
                ops.push(Op {
                    flags: 0,
                    data: new_logged,
                });
            } else {
                ops.push(Op {
                    flags: 0,
                    data: inode_log_format_with_fork(
                        ino,
                        XFS_ILOG_CORE | XFS_ILOG_DDATA,
                        &new_buf,
                        new_dsize as u16,
                    ),
                });
                ops.push(Op {
                    flags: 0,
                    data: new_logged,
                });
                ops.push(Op {
                    flags: 0,
                    data: new_fork_op,
                });
            }
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;

        Ok((ino, lsn))
    }
}

/// The data fork of a directory that has just been made: no entries,
/// and the parent it belongs to.
///
/// Six bytes when the parent's inode number fits in 32 bits and ten when
/// it does not, and which of those it is has to be recorded in
/// `i8count` — a reader takes the width from that field, so a wide
/// parent written as narrow is read as a different directory entirely.
fn empty_short_form_dir(parent: u64) -> Vec<u8> {
    let wide = parent > u64::from(u32::MAX);
    let mut out = Vec::with_capacity(if wide { 10 } else { 6 });
    out.push(0); // count: no entries
    out.push(u8::from(wide)); // i8count: how wide the inode numbers are
    if wide {
        out.extend_from_slice(&parent.to_be_bytes());
    } else {
        out.extend_from_slice(&(parent as u32).to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A created file is a regular file with one link and no contents,
    /// and its generation has moved on from whatever the free inode
    /// carried.
    #[test]
    fn a_created_core_is_an_empty_regular_file() {
        let mut raw = vec![0u8; 176];
        raw[core_at::GEN..core_at::GEN + 4].copy_from_slice(&7u32.to_be_bytes());

        let core = created_core(&raw, 0o100644, Kind::File, 0);
        assert_eq!(
            u16::from_be_bytes(core[core_at::MODE..core_at::MODE + 2].try_into().unwrap()),
            0o100644
        );
        assert_eq!(
            u32::from_be_bytes(core[core_at::NLINK..core_at::NLINK + 4].try_into().unwrap()),
            1
        );
        assert_eq!(
            u64::from_be_bytes(core[core_at::SIZE..core_at::SIZE + 8].try_into().unwrap()),
            0
        );
        assert_eq!(core[core_at::FORMAT], Format::Extents as u8);
        assert_eq!(
            u32::from_be_bytes(core[core_at::GEN..core_at::GEN + 4].try_into().unwrap()),
            8,
            "the generation must move on, so a stale reference to the inode's previous \
             life cannot resolve to its new one"
        );
    }

    /// The identity fields a v5 filesystem checks are carried across
    /// untouched, which is the whole reason the core is read rather than
    /// built.
    #[test]
    fn the_identity_fields_survive() {
        const DI_INO: usize = 152;
        const DI_UUID: usize = 160;

        let mut raw = vec![0u8; 176];
        raw[0..2].copy_from_slice(&0x494eu16.to_be_bytes());
        raw[4] = 3;
        raw[DI_INO..DI_INO + 8].copy_from_slice(&186u64.to_be_bytes());
        raw[DI_UUID..DI_UUID + 16].copy_from_slice(&[0xab; 16]);

        let core = created_core(&raw, 0o100644, Kind::File, 0);
        assert_eq!(&core[0..2], &0x494eu16.to_be_bytes(), "di_magic");
        assert_eq!(core[4], 3, "di_version");
        assert_eq!(
            u64::from_be_bytes(core[DI_INO..DI_INO + 8].try_into().unwrap()),
            186,
            "di_ino"
        );
        assert_eq!(&core[DI_UUID..DI_UUID + 16], &[0xab; 16], "di_uuid");
    }
}
