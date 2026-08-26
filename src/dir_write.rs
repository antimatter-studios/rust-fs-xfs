//! Renaming an entry inside one short-form directory.
//!
//! The smallest change a journalled filesystem can make that is not a
//! single inode's core. Two items, five item operations, no buffer item,
//! no allocator metadata — see `docs/transaction-shapes.md`, where the
//! shapes of twelve operations are compared and this is the one to write
//! first.
//!
//! Nothing here touches the inodes on disk. The record is the durable
//! statement of the change, and whatever mounts the filesystem next
//! applies it — which is also what makes the result checkable, since a
//! replay that did not happen is then distinguishable from one that did.
//!
//! # What it will not do yet
//!
//! Only within one directory, only short form, only when the new name is
//! free. Moving between directories logs the two parents and adjusts
//! link counts; a directory past short form logs a buffer item for the
//! block it lives in. Both are worth doing and neither is this.
//!
//! # The two things the record said that guessing would not have
//!
//! **The fork operation is padded and its recorded size is not.**
//! `ilf_dsize` held 30 for a 30-byte directory whose operation was 32
//! bytes long. Operations round up to four bytes; the fork does not.
//!
//! **A rename appends rather than replacing.** Each entry carries an
//! offset, and they increase through the list. Renaming `aaaa` to `cccc`
//! in a directory holding `aaaa` at `0x60` and `bbbb` at `0x70` left
//! `bbbb` at `0x70` and `cccc` at `0x80` — the old entry removed and a
//! new one appended past everything else, though the replacement name
//! was the same length and would have fit exactly where the old one was.
//! Those offsets are readdir cookies rather than positions in the fork,
//! so reusing one would hand the same cookie to a different entry.

use crate::dir;
use crate::error::{Error, Result};
use crate::format::dir::{XFS_DIR2_DATA_ALIGN, XFS_DIR2_SF_HDR_SIZE_4, XFS_DIR2_SF_HDR_SIZE_8};
use crate::format::log_items::inode_log_format::{XFS_ILOG_CORE, XFS_ILOG_DDATA};
use crate::fs::Filesystem;
use crate::inode::Format;
use crate::log_write::{
    append, inode_log_format, inode_log_format_with_fork, log_dinode_from_disk, trans_header,
    InodeBuffer, Op, XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// Log operations round up to this; the data they carry does not.
const OP_ALIGN: usize = 4;

/// Bytes an entry occupies in the data block a directory becomes when it
/// outgrows the inode.
///
/// Not the entry's size in the fork — this is what the offset cookies
/// are spaced by. An entry there is the inode number, the name's length
/// byte, the name, the file-type byte where the filesystem has one, and
/// a two-byte tag repeating the entry's own position, rounded up to the
/// directory's eight-byte alignment.
fn cookie_span(namelen: usize, has_ftype: bool) -> u32 {
    let raw = 8 + 1 + namelen + usize::from(has_ftype) + 2;
    let aligned = raw.div_ceil(XFS_DIR2_DATA_ALIGN) * XFS_DIR2_DATA_ALIGN;
    aligned as u32
}

impl Filesystem {
    /// Rename `from` to `to` within the directory `dir_ino`.
    ///
    /// Returns the sequence number of the record written. The inodes on
    /// disk are left alone; the change takes effect when the log is
    /// replayed.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`],
    /// [`Error::NotADirectory`], [`Error::NotFound`] if `from` is not
    /// there, [`Error::AlreadyExists`] if `to` is, and
    /// [`Error::UnsupportedFeature`] naming which restriction was met
    /// for a directory this cannot yet rewrite.
    pub fn rename_in_directory(&self, dir_ino: u64, from: &[u8], to: &[u8]) -> Result<u64> {
        self.begin_checkpoint()?;
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if to.is_empty() || to.len() > u8::MAX as usize {
            return Err(Error::UnsupportedFeature(format!(
                "a name of {} bytes cannot be stored; the length is one byte",
                to.len()
            )));
        }

        let (dir, dir_raw) = self.read_inode_raw(dir_ino)?;
        if !dir.is_dir() {
            return Err(Error::NotADirectory);
        }
        if dir.format != Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {dir_ino}: the directory has outgrown the inode, so renaming in it \
                 rewrites a directory block rather than the inode's own fork"
            )));
        }

        let (fork_start, fork_end) = dir.data_fork_range(usize::from(self.sb.inodesize));
        let parsed = dir::read_short_form(&dir, &dir_raw[fork_start..fork_end], &self.sb)?;

        if parsed.entries.iter().any(|e| e.name == to) {
            return Err(Error::AlreadyExists);
        }
        let Some(target) = parsed.entries.iter().find(|e| e.name == from) else {
            return Err(Error::NotFound);
        };
        let moved_ino = target.ino;

        let fork = self.short_form_after_rename(&parsed, from, to, fork_end - fork_start)?;

        // The directory's core changes: its size follows the fork, and
        // its timestamps follow the change.
        let mut dir_core = dir_raw[..].to_vec();
        set_size(&mut dir_core, fork.len() as u64);
        bump_changecount(&mut dir_core, self.sb.is_v5());

        // The renamed inode's core changes only in its timestamps — a
        // rename alters the entry naming it, not the inode. It is logged
        // all the same, because the kernel logs it and a replay that
        // found only one of the two items would leave the pair
        // disagreeing about when the change happened.
        let (_, mut moved_core) = self.read_inode_raw(moved_ino)?;
        bump_changecount(&mut moved_core, self.sb.is_v5());

        let dir_logged = log_dinode_from_disk(&dir_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {dir_ino}: {why}")))?;
        let moved_logged = log_dinode_from_disk(&moved_core)
            .map_err(|why| Error::UnsupportedFeature(format!("inode {moved_ino}: {why}")))?;

        let cluster = self.sb.inode_cluster_bytes();
        let dir_buf = InodeBuffer::containing(self.inode_offset(dir_ino)?, cluster);
        let moved_buf = InodeBuffer::containing(self.inode_offset(moved_ino)?, cluster);

        // The fork travels big-endian inside a native-endian record, and
        // its operation is padded while `ilf_dsize` is not.
        let dsize = fork.len();
        let mut fork_op = fork;
        fork_op.resize(dsize.div_ceil(OP_ALIGN) * OP_ALIGN, 0);

        let device = self.writable.as_ref().expect("checked above");
        append(device.as_ref(), &self.sb, |tid| {
            vec![
                Op {
                    flags: XLOG_START_TRANS,
                    data: Vec::new(),
                },
                Op {
                    flags: 0,
                    // Five item operations: the directory's format, core
                    // and fork, then the moved inode's format and core.
                    data: trans_header(tid, XFS_TRANS_CHECKPOINT, 5),
                },
                Op {
                    flags: 0,
                    data: inode_log_format_with_fork(
                        dir_ino,
                        XFS_ILOG_CORE | XFS_ILOG_DDATA,
                        &dir_buf,
                        dsize as u16,
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
                Op {
                    flags: 0,
                    data: inode_log_format(moved_ino, XFS_ILOG_CORE, &moved_buf),
                },
                Op {
                    flags: 0,
                    data: moved_logged,
                },
                Op {
                    flags: XLOG_COMMIT_TRANS,
                    data: Vec::new(),
                },
            ]
        })
    }

    /// The directory's fork with `from` removed and `to` appended.
    ///
    /// Built rather than edited in place, because an equal-length
    /// replacement still moves: the new entry goes at the end with a
    /// fresh cookie, which is what the kernel does and what keeps the
    /// cookies increasing.
    fn short_form_after_rename(
        &self,
        parsed: &dir::ShortFormDir,
        from: &[u8],
        to: &[u8],
        fork_space: usize,
    ) -> Result<Vec<u8>> {
        let has_ftype = self.sb.has_ftype();
        let next = next_cookie(parsed, has_ftype);

        let moved = parsed
            .entries
            .iter()
            .find(|e| e.name == from)
            .expect("the caller checked this");

        let mut entries: Vec<SfEntry> = parsed
            .entries
            .iter()
            .filter(|e| e.name != from)
            .map(|e| SfEntry {
                name: &e.name,
                ino: e.ino,
                ftype: dir::ftype_to_raw(e.ftype),
                cookie: e.offset,
            })
            .collect();
        entries.push(SfEntry {
            name: to,
            ino: moved.ino,
            ftype: dir::ftype_to_raw(moved.ftype),
            cookie: next,
        });

        encode_short_form(parsed, has_ftype, &entries, fork_space)
    }

    /// The directory's fork with `name` added at the end.
    ///
    /// The new entry goes last with a fresh cookie, the same as a
    /// rename's replacement does, because a cookie is a reader's place
    /// in the directory and handing out one that has been used before
    /// would send a reader that is part-way through back to an entry it
    /// has already seen.
    pub(crate) fn short_form_with_entry(
        &self,
        parsed: &dir::ShortFormDir,
        name: &[u8],
        ino: u64,
        ftype: u8,
        fork_space: usize,
    ) -> Result<Vec<u8>> {
        let has_ftype = self.sb.has_ftype();
        let next = next_cookie(parsed, has_ftype);

        let mut entries: Vec<SfEntry> = parsed
            .entries
            .iter()
            .map(|e| SfEntry {
                name: &e.name,
                ino: e.ino,
                ftype: dir::ftype_to_raw(e.ftype),
                cookie: e.offset,
            })
            .collect();
        entries.push(SfEntry {
            name,
            ino,
            ftype,
            cookie: next,
        });

        encode_short_form(parsed, has_ftype, &entries, fork_space)
    }

    /// The directory's fork with `name` taken out.
    ///
    /// The entries that remain keep their cookies. A cookie is a
    /// reader's place in the directory, and shuffling the survivors down
    /// would move entries a reader part-way through has already passed,
    /// so it would see them twice.
    pub(crate) fn short_form_without_entry(
        &self,
        parsed: &dir::ShortFormDir,
        name: &[u8],
        fork_space: usize,
    ) -> Result<Vec<u8>> {
        let has_ftype = self.sb.has_ftype();
        if !parsed.entries.iter().any(|e| e.name == name) {
            return Err(Error::NotFound);
        }

        let entries: Vec<SfEntry> = parsed
            .entries
            .iter()
            .filter(|e| e.name != name)
            .map(|e| SfEntry {
                name: &e.name,
                ino: e.ino,
                ftype: dir::ftype_to_raw(e.ftype),
                cookie: e.offset,
            })
            .collect();

        encode_short_form(parsed, has_ftype, &entries, fork_space)
    }
}

/// One entry, as the encoder below wants it.
struct SfEntry<'a> {
    name: &'a [u8],
    ino: u64,
    ftype: u8,
    cookie: u32,
}

/// The cookie the next entry appended to this directory should carry.
///
/// One past the highest any existing entry reaches, so it can never
/// collide with one already handed out.
fn next_cookie(parsed: &dir::ShortFormDir, has_ftype: bool) -> u32 {
    parsed
        .entries
        .iter()
        .map(|e| e.offset + cookie_span(e.name.len(), has_ftype))
        .max()
        .unwrap_or(0)
}

/// Encode a short-form directory from its header and a final list of
/// entries.
///
/// Shared by rename and create because the encoding is the same work:
/// what differs is only which entries end up in the list. The entry
/// count comes from the list rather than from `parsed`, which is the one
/// thing that would otherwise be right for a rename and wrong for
/// everything else.
fn encode_short_form(
    parsed: &dir::ShortFormDir,
    has_ftype: bool,
    entries: &[SfEntry],
    fork_space: usize,
) -> Result<Vec<u8>> {
    let wide = parsed.i8count != 0;
    let header = if wide {
        XFS_DIR2_SF_HDR_SIZE_8
    } else {
        XFS_DIR2_SF_HDR_SIZE_4
    };

    let mut out = Vec::with_capacity(fork_space);
    out.push(u8::try_from(entries.len()).map_err(|_| {
        Error::UnsupportedFeature(format!(
            "a short-form directory cannot hold {} entries",
            entries.len()
        ))
    })?);
    out.push(parsed.i8count);
    if wide {
        out.extend_from_slice(&parsed.parent_ino.to_be_bytes());
    } else {
        out.extend_from_slice(&(parsed.parent_ino as u32).to_be_bytes());
    }
    debug_assert_eq!(out.len(), header);

    for e in entries {
        // A cookie is two bytes, so a directory can outgrow the range
        // before it outgrows the inode. Refusing is the only correct
        // answer: a truncated cookie collides with an existing entry's.
        if e.cookie > u32::from(u16::MAX) {
            return Err(Error::UnsupportedFeature(format!(
                "a directory offset of {} is past what the two-byte field holds",
                e.cookie
            )));
        }
        out.push(e.name.len() as u8);
        out.extend_from_slice(&(e.cookie as u16).to_be_bytes());
        out.extend_from_slice(e.name);
        if has_ftype {
            out.push(e.ftype);
        }
        if wide {
            out.extend_from_slice(&e.ino.to_be_bytes());
        } else {
            out.extend_from_slice(&(e.ino as u32).to_be_bytes());
        }
    }

    if out.len() > fork_space {
        return Err(Error::UnsupportedFeature(format!(
            "the directory needs {} bytes and the inode's fork holds {fork_space}; \
             growing it past the inode is not implemented",
            out.len()
        )));
    }
    Ok(out)
}

/// Set `di_size` in an on-disk inode's bytes.
fn set_size(raw: &mut [u8], size: u64) {
    const DI_SIZE: usize = 56;
    raw[DI_SIZE..DI_SIZE + 8].copy_from_slice(&size.to_be_bytes());
}

/// Advance an inode's change counter, the way a metadata change does.
///
/// `di_changecount` is what says this inode has moved on, and the
/// `log_dinode` oracle uses it to tell a record that predates the disk
/// from a conversion fault. Leaving it still would make a rename look,
/// to that test, like a record that had already been applied.
///
/// The timestamps are deliberately left alone. There is no clock here
/// that agrees with the one the filesystem was last written by, an old
/// time is less wrong than an invented one, and nothing in the replay
/// depends on them.
fn bump_changecount(raw: &mut [u8], v5: bool) {
    /// `di_changecount` in the v3 core, immediately after `di_crc`.
    const DI_CHANGECOUNT: usize = 104;

    if !v5 {
        return;
    }
    let at = DI_CHANGECOUNT;
    let now = u64::from_be_bytes(raw[at..at + 8].try_into().expect("8 bytes"));
    raw[at..at + 8].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
}
