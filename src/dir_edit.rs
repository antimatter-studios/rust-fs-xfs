//! Rewriting a directory that has left its inode (#366).
//!
//! A directory in block or leaf form is read whole, changed as a list of
//! entries, and laid out again: in one block if its entries and index fit
//! there, as data blocks and a leaf block if the index fits one leaf, and
//! refused if it needs more than that, which is the node form (#367).
//!
//! The blocks it already has are kept where they are, each verified before
//! it is rebuilt; blocks it now needs are taken from its own allocation
//! group, and blocks it no longer needs go back to free space, their
//! reverse mappings with them. Every rebuilt block is logged as what
//! changed against what was there.
//!
//! Rebuilding keeps every name and inode and the order they were in, and
//! packs them; a cookie a reader took before the change may point
//! somewhere else after it, which is a property of an offline writer this
//! driver already has for block form.

use crate::buf_write::BufferItem;
use crate::dir_block::{self, Entry};
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::format::dir::{
    leaf_first_fsb, offsets, XFS_DIR2_LEAF_OFFSET, XFS_DIR3_BLOCK_MAGIC, XFS_DIR3_DATA_MAGIC,
    XFS_DIR3_LEAF1_MAGIC,
};
use crate::format::log_items::buf_log_format::buf_type::{
    BLFT_DIR_BLOCK, BLFT_DIR_DATA, BLFT_DIR_LEAF1,
};
use crate::fs::Filesystem;
use crate::group_write::{changed_chunks, Allocations};
use crate::inode::{Format, Inode};

/// What a directory became.
pub(crate) struct Rewritten {
    /// Every block written, as logged buffer items.
    pub items: Vec<BufferItem>,
    /// The data fork: the extent list.
    pub fork: Vec<u8>,
    /// `di_size`: the data space, every data block.
    pub size: u64,
    /// `di_nblocks`: data blocks and the leaf, in filesystem blocks.
    pub blocks: u64,
    /// How many extents the fork lists.
    pub nextents: u64,
}

/// Where a directory block is and whether it is the one this directory
/// had there.
#[derive(Clone, Copy)]
struct Placed {
    fsblock: u64,
    existing: bool,
}

impl Filesystem {
    /// Every entry of a directory in block or leaf form, `.` and `..`
    /// first, in the order they are stored.
    pub(crate) fn entries_in_blocks(&self, dir: &Inode, raw: &[u8]) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for (db, fsblock) in self.directory_data_blocks(dir, raw)? {
            let mut block = vec![0u8; self.sb.dirblocksize() as usize];
            self.device()
                .read_at(self.sb.fsblock_offset(fsblock), &mut block)?;
            self.verify_dir_block(&block, fsblock, dir.ino)?;
            let entries = crate::dir::parse_data_block(&block, &self.sb).map_err(|e| {
                Error::UnsupportedFeature(format!(
                    "inode {}'s data block {db} could not be read: {e}",
                    dir.ino
                ))
            })?;
            out.extend(entries.into_iter().map(|e| Entry {
                name: e.name,
                ino: e.ino,
                ftype: crate::dir::ftype_to_raw(e.ftype),
            }));
        }
        let dots = out.len() >= 2 && out[0].name == b"." && out[1].name == b"..";
        if !dots {
            return Err(Error::UnsupportedFeature(format!(
                "inode {}'s first data block does not begin with `.` and `..`",
                dir.ino
            )));
        }
        Ok(out)
    }

    /// The data blocks of a block- or leaf-form directory, as (directory
    /// block number, filesystem block), in order.
    fn directory_data_blocks(&self, dir: &Inode, raw: &[u8]) -> Result<Vec<(u64, u64)>> {
        let per = 1u64 << self.sb.dirblklog;
        let limit = XFS_DIR2_LEAF_OFFSET / u64::from(self.sb.blocksize);
        let mut out = Vec::new();
        for e in self.data_extents(dir, raw)? {
            if e.startoff >= limit {
                continue;
            }
            if e.unwritten || e.startoff % per != 0 || e.blockcount % per != 0 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}'s directory extent at {} is not whole directory blocks",
                    dir.ino, e.startoff
                )));
            }
            for i in 0..e.blockcount / per {
                out.push((e.startoff / per + i, e.startblock + i * per));
            }
        }
        out.sort_unstable();
        for (i, &(db, _)) in out.iter().enumerate() {
            if db != i as u64 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}'s data blocks have a gap at directory block {i}",
                    dir.ino
                )));
            }
        }
        Ok(out)
    }

    /// Lay the directory `ino` out again holding exactly `entries`.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when the entries need the node form,
    /// or more extents than the inode can list, and whatever reading the
    /// blocks it has or taking new ones returns.
    pub(crate) fn rewrite_directory<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        dir: &Inode,
        raw: &[u8],
        entries: &[Entry],
    ) -> Result<Rewritten> {
        let sb = &self.sb;
        let dirblocksize = sb.dirblocksize() as usize;
        let per = 1u64 << sb.dirblklog;
        let block_form = dir_block::space_needed(entries) <= dirblocksize;
        let groups = if block_form {
            vec![entries.to_vec()]
        } else {
            crate::dir_leaf::pack(entries, dirblocksize)?
        };
        if !block_form && entries.len() > crate::dir_leaf::leaf_capacity(dirblocksize, groups.len())
        {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}'s {} entries need more than one leaf block of index; that is \
                 the node form (#367)",
                entries.len()
            )));
        }

        let existing = if dir.format == Format::Extents {
            self.directory_data_blocks(dir, raw)?
        } else {
            Vec::new()
        };
        let leaf_fb = leaf_first_fsb(u64::from(sb.blocksize));
        let existing_leaf = if dir.format == Format::Extents {
            self.data_extents(dir, raw)?
                .into_iter()
                .find(|e| e.startoff == leaf_fb)
                .map(|e| e.startblock)
        } else {
            None
        };
        let (agno, _, _) = sb.split_ino(ino);

        // Where each data block goes: the one it already has, or a new one.
        let mut placed = Vec::with_capacity(groups.len());
        for db in 0..groups.len() as u64 {
            match existing.get(db as usize) {
                Some(&(_, fsblock)) => placed.push(Placed {
                    fsblock,
                    existing: true,
                }),
                None => {
                    let agblock = allocations.group(sb, self.device(), agno)?.take(
                        per as u32,
                        ino as i64,
                        db * per,
                    )?;
                    placed.push(Placed {
                        fsblock: (u64::from(agno) << sb.agblklog) | u64::from(agblock),
                        existing: false,
                    });
                }
            }
        }
        let leaf = if block_form {
            None
        } else {
            Some(match existing_leaf {
                Some(fsblock) => Placed {
                    fsblock,
                    existing: true,
                },
                None => {
                    let agblock = allocations
                        .group(sb, self.device(), agno)?
                        .take(per as u32, ino as i64, leaf_fb)?;
                    Placed {
                        fsblock: (u64::from(agno) << sb.agblklog) | u64::from(agblock),
                        existing: false,
                    }
                }
            })
        };

        // What it no longer needs goes back.
        let mut released: Vec<(u64, u64)> = existing
            .iter()
            .skip(groups.len())
            .map(|&(db, fsblock)| (fsblock, db * per))
            .collect();
        if block_form {
            if let Some(fsblock) = existing_leaf {
                released.push((fsblock, leaf_fb));
            }
        }
        for (fsblock, offset) in released {
            let (ag, agbno) = sb.split_fsblock(fsblock);
            let group = allocations.group(sb, self.device(), ag)?;
            group.forget_rmap(crate::rmap::Rmap {
                startblock: agbno,
                blockcount: per as u32,
                owner: ino as i64,
                offset,
            })?;
            group.give_back(crate::alloc_btree::FreeExtent {
                startblock: agbno,
                blockcount: per as u32,
            })?;
        }

        // The blocks themselves.
        let mut items = Vec::new();
        let mut write = |at: Placed, after: Vec<u8>, kind: u16| -> Result<()> {
            let mut before = vec![0u8; dirblocksize];
            if at.existing {
                self.device()
                    .read_at(sb.fsblock_offset(at.fsblock), &mut before)?;
                // A block kept is a block trusted: one the kernel's
                // verifier refuses is refused here, not written over.
                self.verify_dir_block(&before, at.fsblock, ino)?;
                let magic = crate::endian::be32(&before, 0);
                let da_magic = crate::endian::be16(&before, offsets::da_blk::MAGIC);
                let expected = match kind {
                    BLFT_DIR_LEAF1 => da_magic == XFS_DIR3_LEAF1_MAGIC,
                    _ => matches!(magic, XFS_DIR3_BLOCK_MAGIC | XFS_DIR3_DATA_MAGIC),
                };
                if !expected {
                    return Err(Error::UnsupportedFeature(format!(
                        "inode {ino}'s directory block at {} is not the kind its offset \
                         calls for",
                        at.fsblock
                    )));
                }
            }
            let blkno = crate::alloc_btree::blkno_of_fsbno(sb, at.fsblock);
            items.push(changed_chunks(blkno, &before, after, kind));
            Ok(())
        };
        if block_form {
            let block = dir_block::build(sb, placed[0].fsblock, ino, entries)?;
            write(placed[0], block, BLFT_DIR_BLOCK)?;
        } else {
            let mut index = Vec::with_capacity(entries.len());
            let mut bests = Vec::with_capacity(groups.len());
            for (db, group) in groups.iter().enumerate() {
                let built = crate::dir_leaf::build_data_block(
                    sb,
                    placed[db].fsblock,
                    ino,
                    db as u64,
                    group,
                )?;
                index.extend(built.index);
                bests.push(built.best);
                write(placed[db], built.bytes, BLFT_DIR_DATA)?;
            }
            let at = leaf.expect("leaf form has a leaf");
            let block = crate::dir_leaf::build_leaf(sb, at.fsblock, ino, index, &bests)?;
            write(at, block, BLFT_DIR_LEAF1)?;
        }

        // The extent list: contiguous blocks merged, the leaf last.
        let mut extents: Vec<Extent> = Vec::new();
        for (db, at) in placed.iter().enumerate() {
            let startoff = db as u64 * per;
            match extents.last_mut() {
                Some(e)
                    if e.startoff + e.blockcount == startoff
                        && e.startblock + e.blockcount == at.fsblock =>
                {
                    e.blockcount += per;
                }
                _ => extents.push(Extent {
                    startoff,
                    startblock: at.fsblock,
                    blockcount: per,
                    unwritten: false,
                }),
            }
        }
        if let Some(at) = leaf {
            extents.push(Extent {
                startoff: leaf_fb,
                startblock: at.fsblock,
                blockcount: per,
                unwritten: false,
            });
        }
        let (fork_start, fork_end) = dir.data_fork_range(usize::from(sb.inodesize));
        let room = (fork_end - fork_start) / 16;
        if extents.len() > room {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino}'s directory would need {} extents and its fork lists {room}; \
                 an extent B+tree for a directory is not implemented",
                extents.len()
            )));
        }
        let mut fork = Vec::with_capacity(extents.len() * 16);
        for e in &extents {
            fork.extend_from_slice(&e.to_bytes()?);
        }
        Ok(Rewritten {
            items,
            fork,
            size: groups.len() as u64 * dirblocksize as u64,
            blocks: (groups.len() as u64 + u64::from(leaf.is_some())) * per,
            nextents: extents.len() as u64,
        })
    }
}
