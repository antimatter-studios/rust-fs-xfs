//! Mounted filesystem handle.
//!
//! Ties the parsers together into the operations a consumer actually
//! wants: open a device, resolve a path, list a directory, read a file.
//!
//! # What is deliberately refused
//!
//! A driver that guesses is worse than one that declines, so this
//! refuses rather than approximates in three cases:
//!
//! - **A dirty log.** The log holds committed transactions that have not
//!   reached the metadata. Mounting without replaying it presents a
//!   stale, internally inconsistent tree. Replay is not implemented yet,
//!   so a dirty volume is an error rather than a best effort.
//! - **Real-time inodes.** Their extents live on a separate device this
//!   driver was never handed.
//! - **B+tree-format forks.** Files fragmented past what the inode can
//!   hold need the bmbt walker, which is not written yet. The error says
//!   so instead of returning a truncated file.
//!
//! # Holes and unwritten extents
//!
//! Both read back as zeros. A hole has no extent at all; an unwritten
//! extent has blocks allocated that were never written, and returning
//! their contents would leak whatever previously occupied them. Neither
//! is an edge case to be tidied up later — see [`Filesystem::read_at`].

use crate::ag::{Agf, Agi};
use crate::dir::{self, DirEntry};
use crate::error::{Error, Result};
use crate::extent::{self, Extent};
use crate::inode::{Format, Inode};
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::sync::Arc;

/// File byte offset at which a directory's leaf blocks begin.
///
/// XFS partitions a directory's file-offset space into three regions —
/// data, leaf, then free — each 32 GiB wide, so that the leaf index can
/// grow without colliding with the entry data below it. The regions are
/// address space, not allocated blocks: a small directory occupies a few
/// blocks at the bottom of the data region and nothing else.
///
/// Only the data region holds entries, so a directory scan stops here
/// rather than walking into the index.
const DIR_LEAF_FILE_OFFSET: u64 = 1 << 35;

/// A mounted XFS filesystem.
pub struct Filesystem {
    device: Arc<dyn BlockRead>,
    sb: Superblock,
}

impl Filesystem {
    /// Open `device` as an XFS filesystem.
    ///
    /// # Errors
    ///
    /// [`Error::NotXfs`] if the superblock magic does not match,
    /// [`Error::DirtyLog`] if the log needs replaying, and any parse
    /// failure from the superblock itself.
    pub fn mount(device: Arc<dyn BlockRead>) -> Result<Self> {
        // The superblock lives in the first sector. Read a generous
        // fixed amount: the sector size is not known until it has been
        // parsed, and 4 KiB covers every sector size XFS supports.
        let mut buf = vec![0u8; 4096];
        device.read_at(0, &mut buf)?;
        let sb = Superblock::parse(&buf)?;

        let fs = Filesystem { device, sb };
        fs.check_log_is_clean()?;
        Ok(fs)
    }

    /// The parsed superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// Refuse a volume whose log holds unapplied transactions.
    ///
    /// Detected through the AGI unlinked lists rather than by reading
    /// the log itself: an inode that is unlinked but still open is
    /// recorded there, and on a cleanly unmounted filesystem every
    /// bucket is empty. This is a conservative check — it catches the
    /// case that matters for correctness (metadata the log still owns)
    /// without pretending to understand log records.
    ///
    /// A full implementation replays the log instead of refusing. Until
    /// that exists, refusing is the honest behaviour.
    fn check_log_is_clean(&self) -> Result<()> {
        for ag in 0..self.sb.agcount {
            let agi = self.read_agi(ag)?;
            if agi.has_unlinked_inodes() {
                return Err(Error::DirtyLog);
            }
        }
        Ok(())
    }

    /// Byte offset of a filesystem block.
    ///
    /// Delegates to the superblock because an XFS block number is packed
    /// as (allocation group, block within group) rather than being a
    /// linear device index.
    fn block_offset(&self, block: u64) -> u64 {
        self.sb.fsblock_offset(block)
    }

    /// Byte offset of the start of an allocation group.
    fn ag_offset(&self, ag: u32) -> u64 {
        u64::from(ag) * u64::from(self.sb.agblocks) * u64::from(self.sb.blocksize)
    }

    /// Read and validate an AGF.
    pub fn read_agf(&self, ag: u32) -> Result<Agf> {
        let mut buf = vec![0u8; usize::from(self.sb.sectsize)];
        // Sector 0 of an AG is its superblock copy, sector 1 the AGF.
        self.device
            .read_at(self.ag_offset(ag) + u64::from(self.sb.sectsize), &mut buf)?;
        Agf::parse(&buf, &self.sb, ag)
    }

    /// Read and validate an AGI.
    pub fn read_agi(&self, ag: u32) -> Result<Agi> {
        let mut buf = vec![0u8; usize::from(self.sb.sectsize)];
        // Sector 2 of an AG is its AGI.
        self.device.read_at(
            self.ag_offset(ag) + 2 * u64::from(self.sb.sectsize),
            &mut buf,
        )?;
        Agi::parse(&buf, &self.sb, ag)
    }

    /// Byte offset of an inode, derived from its number.
    fn inode_offset(&self, ino: u64) -> Result<u64> {
        let (ag, ag_block, offset) = self.sb.split_ino(ino);
        if ag >= self.sb.agcount {
            return Err(Error::BadSuperblock(format!(
                "inode {ino} names allocation group {ag}, but there are only {}",
                self.sb.agcount
            )));
        }
        Ok(self.ag_offset(ag)
            + u64::from(ag_block) * u64::from(self.sb.blocksize)
            + u64::from(offset) * u64::from(self.sb.inodesize))
    }

    /// Read one inode by number, with its raw record.
    ///
    /// The record is returned alongside the parsed core because the
    /// forks live inside it and callers need both.
    pub fn read_inode_raw(&self, ino: u64) -> Result<(Inode, Vec<u8>)> {
        let mut buf = vec![0u8; usize::from(self.sb.inodesize)];
        self.device.read_at(self.inode_offset(ino)?, &mut buf)?;
        let inode = Inode::parse(&buf, &self.sb, ino)?;
        Ok((inode, buf))
    }

    /// Read one inode by number.
    pub fn read_inode(&self, ino: u64) -> Result<Inode> {
        Ok(self.read_inode_raw(ino)?.0)
    }

    /// The root directory inode.
    pub fn root_inode(&self) -> Result<Inode> {
        self.read_inode(self.sb.rootino)
    }

    /// The data fork's extent list.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for a B+tree-format fork, which
    /// needs the bmbt walker, or for a real-time inode, whose extents
    /// are on a device this driver does not have.
    fn data_extents(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<Extent>> {
        if inode.is_realtime() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} keeps its data on the real-time device",
                inode.ino
            )));
        }
        match inode.format {
            Format::Extents => {
                let (start, end) = inode.data_fork_range(usize::from(self.sb.inodesize));
                extent::parse_list(&raw[start..end], inode.nextents)
            }
            Format::Btree => Err(Error::UnsupportedFeature(format!(
                "inode {} has a B+tree-format data fork; the bmbt walker is not implemented",
                inode.ino
            ))),
            other => Err(Error::UnsupportedFeature(format!(
                "inode {} has a {other:?}-format data fork, which holds no extents",
                inode.ino
            ))),
        }
    }

    /// Read `buf.len()` bytes from `inode` starting at byte `offset`.
    ///
    /// Returns the number of bytes read, which is short only at end of
    /// file. Holes and unwritten extents produce zeros rather than stale
    /// disk contents.
    pub fn read_at(&self, inode: &Inode, raw: &[u8], offset: u64, buf: &mut [u8]) -> Result<usize> {
        if !inode.is_regular_file() && !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        if offset >= inode.size {
            return Ok(0);
        }

        // A short symlink and a small file can live inline in the inode
        // rather than in any extent.
        if inode.format == Format::Local {
            let (start, end) = inode.data_fork_range(usize::from(self.sb.inodesize));
            let inline = &raw[start..end];
            let len = (inode.size as usize).min(inline.len());
            let from = (offset as usize).min(len);
            let n = buf.len().min(len - from);
            buf[..n].copy_from_slice(&inline[from..from + n]);
            return Ok(n);
        }

        let extents = self.data_extents(inode, raw)?;
        let block_size = u64::from(self.sb.blocksize);
        let want = buf.len().min((inode.size - offset) as usize);

        // Start from zeros so holes and unwritten extents need no
        // special-casing on the copy path — they are simply left alone.
        buf[..want].fill(0);

        let mut done = 0usize;
        while done < want {
            let pos = offset + done as u64;
            let file_block = pos / block_size;
            let within = (pos % block_size) as usize;
            let chunk = (block_size as usize - within).min(want - done);

            if let Some(e) = extent::lookup(&extents, file_block) {
                if !e.is_unwritten() {
                    let phys = e
                        .map(file_block)
                        .expect("lookup returned a covering extent");
                    let at = self.block_offset(phys) + within as u64;
                    self.device.read_at(at, &mut buf[done..done + chunk])?;
                }
                // An unwritten extent has blocks allocated but never
                // written. Returning what they hold would leak the
                // previous owner's data, so it stays zeroed.
            }
            // No extent at all is a hole, which also reads as zeros.
            done += chunk;
        }
        Ok(want)
    }

    /// Read a whole file into a new buffer.
    pub fn read_file(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<u8>> {
        let mut out = vec![0u8; inode.size as usize];
        let n = self.read_at(inode, raw, 0, &mut out)?;
        out.truncate(n);
        Ok(out)
    }

    /// Resolve a symbolic link's target.
    pub fn read_link(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<u8>> {
        if !inode.is_symlink() {
            return Err(Error::NotAFile);
        }
        self.read_file(inode, raw)
    }

    /// List a directory's entries.
    ///
    /// Handles the short-form case inline and reads directory data
    /// blocks through the extent list for every other format.
    ///
    /// `.` and `..` are never returned, in any format. Short form does
    /// not store them at all — the parent lives in its header — while
    /// the block and leaf formats do, so returning whatever the format
    /// happened to hold would make a directory's listing change shape as
    /// it grew past short form. A caller should not have to know how a
    /// directory is stored.
    pub fn read_dir(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<DirEntry>> {
        if !inode.is_dir() {
            return Err(Error::NotADirectory);
        }

        if inode.format == Format::Local {
            let (start, end) = inode.data_fork_range(usize::from(self.sb.inodesize));
            let sf = dir::read_short_form(inode, &raw[start..end], &self.sb)?;
            return Ok(sf.entries);
        }

        let extents = self.data_extents(inode, raw)?;
        let block_size = u64::from(self.sb.blocksize);
        let dir_block_size = self.sb.dirblocksize() as usize;
        let blocks_per_dir_block = (dir_block_size as u64) / block_size;

        // Only the data region holds entries; the leaf index and free
        // space sit above it in the file's offset space.
        let data_block_limit = DIR_LEAF_FILE_OFFSET / block_size;

        // Walk the extents themselves rather than scanning the offset
        // space. The regions are 32 GiB apart, so scanning would step
        // through millions of empty block numbers to reach the leaf.
        let mut out = Vec::new();
        for e in &extents {
            if e.startoff >= data_block_limit {
                break; // into the leaf region; no entries above here
            }
            if e.is_unwritten() {
                continue;
            }
            let last = e.end_offset().min(data_block_limit);
            let mut file_block = e.startoff;
            while file_block < last {
                let phys = e.map(file_block).expect("block inside its own extent");
                let mut block = vec![0u8; dir_block_size];
                self.device.read_at(self.block_offset(phys), &mut block)?;

                // A block in the data region may still be free space
                // rather than entries. parse_data_block rejects those by
                // magic, and that rejection is not fatal to the listing.
                if let Ok(entries) = dir::parse_data_block(&block, &self.sb) {
                    // Block and leaf formats store `.` and `..` as real
                    // entries; short form keeps the parent in its header
                    // and never materialises either. Filtering here keeps
                    // the contract uniform, so a caller does not see a
                    // directory's listing change shape purely because it
                    // grew past short form.
                    out.extend(
                        entries
                            .into_iter()
                            .filter(|e| e.name != b"." && e.name != b".."),
                    );
                }
                file_block += blocks_per_dir_block;
            }
        }
        Ok(out)
    }

    /// Look up a single name within a directory.
    pub fn lookup(&self, dir_inode: &Inode, raw: &[u8], name: &[u8]) -> Result<Inode> {
        let entries = self.read_dir(dir_inode, raw)?;
        let hit = entries
            .iter()
            .find(|e| e.name == name)
            .ok_or(Error::NotFound)?;
        self.read_inode(hit.ino)
    }

    /// Resolve an absolute path to its inode.
    ///
    /// Symbolic links are **not** followed. A caller that wants them
    /// followed should do so explicitly, so that link loops are its
    /// policy rather than a surprise from this function.
    pub fn lookup_path(&self, path: &str) -> Result<Inode> {
        let (mut inode, mut raw) = self.read_inode_raw(self.sb.rootino)?;
        for component in path.split('/').filter(|c| !c.is_empty() && *c != ".") {
            if component == ".." {
                return Err(Error::UnsupportedFeature(
                    "`..` in a path is not resolved by lookup_path".into(),
                ));
            }
            if !inode.is_dir() {
                return Err(Error::NotADirectory);
            }
            let next = self.lookup(&inode, &raw, component.as_bytes())?;
            let (i, r) = self.read_inode_raw(next.ino)?;
            inode = i;
            raw = r;
        }
        Ok(inode)
    }

    /// Read a whole file by path.
    pub fn read_path(&self, path: &str) -> Result<Vec<u8>> {
        let (inode, raw) = {
            let inode = self.lookup_path(path)?;
            self.read_inode_raw(inode.ino)?
        };
        self.read_file(&inode, &raw)
    }

    /// List a directory by path.
    pub fn list_path(&self, path: &str) -> Result<Vec<DirEntry>> {
        let inode = self.lookup_path(path)?;
        let (inode, raw) = self.read_inode_raw(inode.ino)?;
        self.read_dir(&inode, &raw)
    }
}
