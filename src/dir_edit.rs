//! Changing a directory that has left its inode (#366, #367).
//!
//! A directory in block or leaf form is read whole, changed as a list of
//! entries, and laid out again: in one block if its entries and index fit
//! there, as data blocks and a leaf block if the index fits one leaf, and
//! in node form if it does not.
//!
//! A directory already in node form is too big to lay out again for every
//! name, so its edits are made in place, block by block
//! ([`crate::dir_node`]); one that shrinks back to a single leaf's worth
//! is laid out again in leaf form.
//!
//! The blocks it already has are kept where they are, each verified before
//! it is rebuilt; blocks it now needs are taken from its own allocation
//! group first, and blocks it no longer needs go back to free space, their
//! reverse mappings with them. Every rebuilt block is logged as what
//! changed against what was there, and the fork that maps them is an
//! extent list or a block-map B+tree, whichever the count calls for.
//!
//! Laying out again keeps every name and inode and the order they were
//! in, and packs them; a cookie a reader took before the change may point
//! somewhere else after it, which is a property of an offline writer this
//! driver already has for block form.

use crate::buf_write::BufferItem;
use crate::dir_block::Entry;
use crate::dir_node::DirBlocks;
use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::group_write::Allocations;
use crate::inode::{Format, Inode};

/// One change to a directory's names.
pub(crate) enum DirEdit<'a> {
    /// Take this name out; [`Error::NotFound`] when it is not there.
    Remove(&'a [u8]),
    /// Put this entry in; [`Error::AlreadyExists`] when its name is.
    Add(Entry),
    /// Point `..` at another directory.
    Reparent(u64),
    /// Give the entry named the first the second name, keeping its inode
    /// and type: [`Error::NotFound`] and [`Error::AlreadyExists`] as for a
    /// removal and an addition.
    Rename(&'a [u8], &'a [u8]),
}

/// What a directory became.
pub(crate) struct Rewritten {
    /// Every block written, as logged buffer items.
    pub items: Vec<BufferItem>,
    /// The data fork as the inode stores it.
    pub fork: Vec<u8>,
    /// The data fork as the inode item logs it, which for a B+tree root
    /// is another shape.
    pub logged_fork: Vec<u8>,
    /// `XFS_ILOG_DEXT` or `XFS_ILOG_DBROOT`.
    pub fields: u32,
    /// `di_format`: extents or B+tree.
    pub format: Format,
    /// `di_size`: up to the end of the last data block.
    pub size: u64,
    /// `di_nblocks`: every directory block and every block of its map.
    pub blocks: u64,
    /// How many extents map it.
    pub nextents: u64,
}

impl Filesystem {
    /// Every entry of a directory in block, leaf or node form, `.` and
    /// `..` first, in the order they are stored.
    pub(crate) fn entries_in_blocks(&self, dir: &Inode, raw: &[u8]) -> Result<Vec<Entry>> {
        DirBlocks::open(self, dir.ino, dir, raw)?.entries()
    }

    /// Make `edits`, in order, to the directory `ino`, which has left its
    /// inode.
    ///
    /// # Errors
    ///
    /// [`Error::AlreadyExists`] and [`Error::NotFound`] as each edit says,
    /// [`Error::UnsupportedFeature`] for a directory this cannot change,
    /// and whatever reading its blocks or taking new ones returns.
    pub(crate) fn edit_directory<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        dir: &Inode,
        raw: &[u8],
        edits: &[DirEdit<'_>],
    ) -> Result<Rewritten> {
        let mut blocks = DirBlocks::open(self, ino, dir, raw)?;
        if blocks.is_node() {
            blocks.edit(allocations, edits)?;
            if let Some(entries) = blocks.fits_leaf_form()? {
                blocks.lay_out(allocations, &entries)?;
            }
            return blocks.finish(allocations, dir, raw);
        }
        let mut entries = blocks.entries()?;
        for edit in edits {
            match edit {
                DirEdit::Remove(name) => {
                    let had = entries.len();
                    entries.retain(|e| e.name != *name);
                    if entries.len() == had {
                        return Err(Error::NotFound);
                    }
                }
                DirEdit::Add(entry) => {
                    if entries.iter().any(|e| e.name == entry.name) {
                        return Err(Error::AlreadyExists);
                    }
                    entries.push(entry.clone());
                }
                DirEdit::Reparent(parent) => entries[1].ino = *parent,
                DirEdit::Rename(from, to) => {
                    if entries.iter().any(|e| e.name == *to) {
                        return Err(Error::AlreadyExists);
                    }
                    let at = entries
                        .iter()
                        .skip(2)
                        .position(|e| e.name == *from)
                        .ok_or(Error::NotFound)?;
                    let mut moved = entries.remove(at + 2);
                    moved.name = to.to_vec();
                    entries.push(moved);
                }
            }
        }
        blocks.lay_out(allocations, &entries)?;
        blocks.finish(allocations, dir, raw)
    }

    /// Lay the directory `ino` out again holding exactly `entries`.
    ///
    /// # Errors
    ///
    /// Whatever reading the blocks it has or taking new ones returns.
    pub(crate) fn rewrite_directory<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        dir: &Inode,
        raw: &[u8],
        entries: &[Entry],
    ) -> Result<Rewritten> {
        let mut blocks = DirBlocks::open(self, ino, dir, raw)?;
        blocks.lay_out(allocations, entries)?;
        blocks.finish(allocations, dir, raw)
    }
}
