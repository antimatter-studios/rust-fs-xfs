//! Node-form directories (#367).
//!
//! A directory whose hash index no longer fits one leaf block keeps it in
//! a B+tree instead: **leafn** blocks of index records, sorted by hash and
//! threaded left to right, under **node** blocks of `(hash, child)` pairs,
//! whose root is always the first block of the leaf space. The longest free
//! region of each data block moves out of the leaf into **free** blocks of
//! their own, at `XFS_DIR2_FREE_OFFSET`.
//!
//! ```text
//! leafn block  0  blkinfo   56 bytes: siblings, magic 0x3dff, checksum,
//!                           own address, sequence number, UUID, owner
//!             56  count, stale
//!             64  records   8 bytes each: hash, address
//!
//! node block   0  blkinfo   56 bytes, magic 0x3ebe
//!             56  count, level
//!             64  entries   8 bytes each: the highest hash below, and
//!                           the child's block in the directory's file
//!
//! free block   0  header    48 bytes: magic XDF3, checksum, own address,
//!                           sequence number, UUID, owner
//!             48  firstdb, nvalid, nused
//!             64  bests     2 bytes per data block, 0xffff for a hole
//! ```
//!
//! # Edited where it changes, not laid out again
//!
//! Block and leaf form are small enough to rebuild whole on every change
//! (#366); a node-form directory is not, so a name is added or removed by
//! changing only the blocks it touches, as the kernel does: the data block
//! it goes in or comes out of, the free block that block's best is kept
//! in, and the leaf its record is in, with that leaf split when it is
//! full, each node above it split in turn when that is, and a root that
//! splits moved down a level so the root stays where lookups start.
//!
//! The other way, an emptied leaf or node is unlinked from its siblings
//! and given back, a root left with one child takes that child's place,
//! and an emptied data block is given back with its best set to a hole,
//! the directory's size shrinking only when it was the last. A leaf that
//! is merely under-full is left so: the kernel joins siblings to save
//! space, and the result is a valid tree either way.
//!
//! # Moving between forms
//!
//! When a leaf-form directory's index outgrows its leaf, and when a
//! node-form directory is down to one leaf whose records would fit a
//! leaf-form leaf again, it is laid out whole in the form it needs
//! ([`DirBlocks::lay_out`]), which is bounded by one leaf's worth of
//! names.
//!
//! # The checksums are not computed
//!
//! As for every logged block in this driver, recovery recomputes them.

use std::collections::BTreeMap;

use crate::dir_block::{self, entry_size, hash_for, Entry};
use crate::dir_edit::{DirEdit, Rewritten};
use crate::endian::{be16, be32};
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::format::dir::{
    free_first_fsb, leaf_first_fsb, offsets, XFS_DA3_NODE_MAGIC, XFS_DA_NODE_MAXDEPTH,
    XFS_DIR2_DATA_ALIGN, XFS_DIR2_DATA_FREE_NULL, XFS_DIR2_DATA_FREE_TAG, XFS_DIR2_NULL_DATAPTR,
    XFS_DIR3_BLOCK_MAGIC, XFS_DIR3_DATA_HDR_SIZE, XFS_DIR3_DATA_MAGIC, XFS_DIR3_FREE_HDR_SIZE,
    XFS_DIR3_FREE_MAGIC, XFS_DIR3_LEAF1_MAGIC, XFS_DIR3_LEAFN_MAGIC,
};
use crate::format::log_items::buf_log_format::buf_type::{
    BLFT_DA_NODE, BLFT_DIR_BLOCK, BLFT_DIR_DATA, BLFT_DIR_FREE, BLFT_DIR_LEAF1, BLFT_DIR_LEAFN,
};
use crate::fs::Filesystem;
use crate::group_write::{changed_chunks, Allocations};
use crate::inode::Inode;
use crate::superblock::Superblock;

/// `xfs_da3_blkinfo` and the count after it: where records start in a
/// leaf or node block.
const DA_HDR: usize = 64;
/// One index record, or one node entry.
const RECORD: usize = 8;
/// One `bests` entry.
const BEST: usize = 2;

/// A record of the hash index: a name's hash and where its entry is.
type Record = (u32, u32);

/// One block as an edit has it: where it lives, what it held, and what
/// it holds now.
struct Cached {
    fsblock: u64,
    before: Vec<u8>,
    after: Vec<u8>,
}

/// The way from the root to one leaf: each node passed and the entry
/// taken in it.
#[derive(Clone)]
struct Path {
    nodes: Vec<(u64, usize)>,
    leaf: u64,
}

/// A name found in the index.
struct Found {
    path: Path,
    index: usize,
    db: u64,
    offset: usize,
}

/// Every block of one directory, as an edit reads and changes them.
///
/// Blocks are named by where they are in the directory's file, in
/// filesystem blocks (`xfs_dablk_t`), which is also what node entries
/// and sibling pointers hold.
pub(crate) struct DirBlocks<'f> {
    fs: &'f Filesystem,
    ino: u64,
    /// Filesystem blocks per directory block.
    per: u64,
    /// Directory block size in bytes.
    dbs: usize,
    map: BTreeMap<u64, u64>,
    cache: BTreeMap<u64, Cached>,
    /// `di_size`.
    size: u64,
}

impl<'f> DirBlocks<'f> {
    /// The blocks of directory `ino`, as its fork maps them.
    pub(crate) fn open(fs: &'f Filesystem, ino: u64, dir: &Inode, raw: &[u8]) -> Result<Self> {
        let sb = &fs.sb;
        let per = 1u64 << sb.dirblklog;
        let mut map = BTreeMap::new();
        if matches!(
            dir.format,
            crate::inode::Format::Extents | crate::inode::Format::Btree
        ) {
            for e in fs.data_extents(dir, raw)? {
                if e.unwritten || e.startoff % per != 0 || e.blockcount % per != 0 {
                    return Err(Error::UnsupportedFeature(format!(
                        "inode {ino}'s directory extent at {} is not whole directory blocks",
                        e.startoff
                    )));
                }
                for i in 0..e.blockcount / per {
                    map.insert(e.startoff + i * per, e.startblock + i * per);
                }
            }
        }
        Ok(Self {
            fs,
            ino,
            per,
            dbs: sb.dirblocksize() as usize,
            map,
            cache: BTreeMap::new(),
            size: dir.size,
        })
    }

    fn sb(&self) -> &'f Superblock {
        &self.fs.sb
    }

    fn leaf_da(&self) -> u64 {
        leaf_first_fsb(u64::from(self.sb().blocksize))
    }

    fn free_da(&self) -> u64 {
        free_first_fsb(u64::from(self.sb().blocksize))
    }

    fn index_capacity(&self) -> usize {
        (self.dbs - DA_HDR) / RECORD
    }

    fn bests_per_block(&self) -> usize {
        offsets::free_hdr::max_bests(self.dbs, true)
    }

    /// Whether the directory keeps a free index, which only node form does.
    pub(crate) fn is_node(&self) -> bool {
        self.map.range(self.free_da()..).next().is_some()
    }

    /// The data blocks there are, as directory block numbers.
    fn data_blocks(&self) -> Vec<u64> {
        self.map
            .range(..self.leaf_da())
            .map(|(&da, _)| da / self.per)
            .collect()
    }

    fn corrupt(&self, why: impl std::fmt::Display) -> Error {
        Error::UnsupportedFeature(format!("inode {}'s directory: {why}", self.ino))
    }

    /// The block at `dablk`, read and checked the first time it is asked
    /// for: its checksum and identity, and a directory block's magic.
    fn get(&mut self, dablk: u64) -> Result<&mut Vec<u8>> {
        if !self.cache.contains_key(&dablk) {
            let fsblock = *self
                .map
                .get(&dablk)
                .ok_or_else(|| self.corrupt(format!("block {dablk} is not mapped")))?;
            let mut buf = vec![0u8; self.dbs];
            self.fs
                .device()
                .read_at(self.sb().fsblock_offset(fsblock), &mut buf)?;
            self.fs.verify_dir_block(&buf, fsblock, self.ino)?;
            if kind_of(&buf).is_none() {
                return Err(self.corrupt(format!(
                    "block {dablk} at {fsblock} is not a directory block"
                )));
            }
            self.cache.insert(
                dablk,
                Cached {
                    fsblock,
                    before: buf.clone(),
                    after: buf,
                },
            );
        }
        Ok(&mut self.cache.get_mut(&dablk).expect("just put").after)
    }

    fn put(&mut self, dablk: u64, bytes: Vec<u8>) {
        self.cache
            .get_mut(&dablk)
            .expect("read or added first")
            .after = bytes;
    }

    fn fsblock(&self, dablk: u64) -> u64 {
        self.map[&dablk]
    }

    /// A block for `dablk`, from the directory's own group first.
    fn add(&mut self, allocations: &mut Allocations<'f>, dablk: u64) -> Result<u64> {
        let sb = self.sb();
        let (home, _, _) = sb.split_ino(self.ino);
        let mut last = None;
        for i in 0..sb.agcount {
            let agno = (home + i) % sb.agcount;
            match allocations.group(sb, self.fs.device(), agno)?.take(
                self.per as u32,
                self.ino as i64,
                dablk,
            ) {
                Ok(agblock) => {
                    let fsblock = (u64::from(agno) << sb.agblklog) | u64::from(agblock);
                    self.map.insert(dablk, fsblock);
                    self.cache.insert(
                        dablk,
                        Cached {
                            fsblock,
                            before: vec![0; self.dbs],
                            after: vec![0; self.dbs],
                        },
                    );
                    return Ok(fsblock);
                }
                Err(e @ Error::UnsupportedFeature(_)) => last = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last.unwrap_or_else(|| self.corrupt("no allocation group")))
    }

    /// Give the block at `dablk` back, and its reverse mapping with it.
    fn release(&mut self, allocations: &mut Allocations<'f>, dablk: u64) -> Result<()> {
        let fsblock = self
            .map
            .remove(&dablk)
            .ok_or_else(|| self.corrupt(format!("block {dablk} is not mapped")))?;
        self.cache.remove(&dablk);
        let sb = self.sb();
        let (ag, agbno) = sb.split_fsblock(fsblock);
        let group = allocations.group(sb, self.fs.device(), ag)?;
        group.forget_rmap(crate::rmap::Rmap {
            startblock: agbno,
            blockcount: self.per as u32,
            owner: self.ino as i64,
            offset: dablk,
        })?;
        group.give_back(crate::alloc_btree::FreeExtent {
            startblock: agbno,
            blockcount: self.per as u32,
        })?;
        Ok(())
    }

    /// Every entry, `.` and `..` first, data block by data block.
    pub(crate) fn entries(&mut self) -> Result<Vec<Entry>> {
        let mut out = Vec::new();
        for db in self.data_blocks() {
            out.extend(self.data_entries(db)?.into_iter().map(|(_, e)| e));
        }
        let dots = out.len() >= 2 && out[0].name == b"." && out[1].name == b"..";
        if !dots {
            return Err(self.corrupt("its first data block does not begin with `.` and `..`"));
        }
        Ok(out)
    }

    /// The entries of data block `db`, each with its offset in the block.
    fn data_entries(&mut self, db: u64) -> Result<Vec<(usize, Entry)>> {
        let sb = self.sb();
        let ino = self.ino;
        let block = self.get(db * self.per)?;
        let parsed = crate::dir::parse_data_block(block, sb).map_err(|e| {
            Error::UnsupportedFeature(format!(
                "inode {ino}'s data block {db} could not be read: {e}"
            ))
        })?;
        Ok(parsed
            .into_iter()
            .map(|e| {
                (
                    e.offset as usize,
                    Entry {
                        name: e.name,
                        ino: e.ino,
                        ftype: crate::dir::ftype_to_raw(e.ftype),
                    },
                )
            })
            .collect())
    }

    // ------------------------------------------------------------------
    // The index
    // ------------------------------------------------------------------

    /// Records of the leaf at `dablk`, stale ones included.
    fn leaf_records(&mut self, dablk: u64) -> Result<Vec<Record>> {
        let cap = self.index_capacity();
        let b = self.get(dablk)?;
        if be16(b, offsets::da_blk::MAGIC) != XFS_DIR3_LEAFN_MAGIC {
            return Err(self.corrupt(format!("block {dablk} is not an index leaf")));
        }
        let b = self.get(dablk)?;
        let count = usize::from(be16(b, offsets::leaf_hdr::count(true)));
        if count > cap {
            return Err(self.corrupt(format!("leaf {dablk} claims {count} records")));
        }
        Ok((0..count)
            .map(|i| {
                let at = DA_HDR + i * RECORD;
                (be32(b, at), be32(b, at + 4))
            })
            .collect())
    }

    /// The level and entries of the node at `dablk`.
    fn node_entries(&mut self, dablk: u64) -> Result<(u16, Vec<Record>)> {
        let cap = self.index_capacity();
        let b = self.get(dablk)?;
        let count = usize::from(be16(b, offsets::node_hdr::count(true)));
        let level = be16(b, offsets::node_hdr::level(true));
        if be16(b, offsets::da_blk::MAGIC) != XFS_DA3_NODE_MAGIC
            || count == 0
            || count > cap
            || level == 0
            || level > XFS_DA_NODE_MAXDEPTH
        {
            return Err(self.corrupt(format!(
                "block {dablk} is not a node of 1 to {cap} entries (count {count}, level {level})"
            )));
        }
        let b = self.get(dablk)?;
        Ok((
            level,
            (0..count)
                .map(|i| {
                    let at = DA_HDR + i * RECORD;
                    (be32(b, at), be32(b, at + 4))
                })
                .collect(),
        ))
    }

    fn siblings(&mut self, dablk: u64) -> Result<(u32, u32)> {
        let b = self.get(dablk)?;
        Ok((
            be32(b, offsets::da_blk::BACK),
            be32(b, offsets::da_blk::FORW),
        ))
    }

    fn write_leaf(&mut self, dablk: u64, back: u32, forw: u32, records: &[Record]) {
        let block = index_block(
            self.sb(),
            self.fsblock(dablk),
            self.ino,
            self.dbs,
            XFS_DIR3_LEAFN_MAGIC,
            (back, forw),
            None,
            records,
        );
        self.put(dablk, block);
    }

    fn write_node(&mut self, dablk: u64, back: u32, forw: u32, level: u16, entries: &[Record]) {
        let block = index_block(
            self.sb(),
            self.fsblock(dablk),
            self.ino,
            self.dbs,
            XFS_DA3_NODE_MAGIC,
            (back, forw),
            Some(level),
            entries,
        );
        self.put(dablk, block);
    }

    fn set_back(&mut self, dablk: u64, back: u32) -> Result<()> {
        let b = self.get(dablk)?;
        b[offsets::da_blk::BACK..offsets::da_blk::BACK + 4].copy_from_slice(&back.to_be_bytes());
        Ok(())
    }

    fn set_forw(&mut self, dablk: u64, forw: u32) -> Result<()> {
        let b = self.get(dablk)?;
        b[offsets::da_blk::FORW..offsets::da_blk::FORW + 4].copy_from_slice(&forw.to_be_bytes());
        Ok(())
    }

    /// The lowest unused block of the leaf space after the root.
    fn new_index_block(&mut self, allocations: &mut Allocations<'f>) -> Result<u64> {
        let mut dablk = self.leaf_da() + self.per;
        while self.map.contains_key(&dablk) {
            dablk += self.per;
        }
        if dablk >= self.free_da() || u32::try_from(dablk).is_err() {
            return Err(self.corrupt("its index has no room left in its leaf space"));
        }
        self.add(allocations, dablk)?;
        Ok(dablk)
    }

    /// From the root to the first leaf that can hold `hash`: in each node
    /// the leftmost entry whose hash is at least it, or the last.
    fn descend(&mut self, hash: u32) -> Result<Path> {
        let mut nodes = Vec::new();
        let mut at = self.leaf_da();
        let mut above = XFS_DA_NODE_MAXDEPTH + 1;
        for _ in 0..=XFS_DA_NODE_MAXDEPTH {
            let magic = be16(self.get(at)?, offsets::da_blk::MAGIC);
            if magic == XFS_DIR3_LEAFN_MAGIC {
                return Ok(Path { nodes, leaf: at });
            }
            let (level, entries) = self.node_entries(at)?;
            if level >= above {
                return Err(self.corrupt(format!("node {at} is not below its parent")));
            }
            above = level;
            let i = entries
                .iter()
                .position(|&(h, _)| h >= hash)
                .unwrap_or(entries.len() - 1);
            nodes.push((at, i));
            at = u64::from(entries[i].1);
        }
        Err(self.corrupt("its index is deeper than XFS allows"))
    }

    /// Move `path` to the next leaf to the right, through the tree rather
    /// than the sibling chain. False at the last.
    fn next_leaf(&mut self, path: &mut Path) -> Result<bool> {
        while let Some((node, i)) = path.nodes.pop() {
            let (_, entries) = self.node_entries(node)?;
            if i + 1 < entries.len() {
                path.nodes.push((node, i + 1));
                let mut at = u64::from(entries[i + 1].1);
                for _ in 0..=XFS_DA_NODE_MAXDEPTH {
                    let magic = be16(self.get(at)?, offsets::da_blk::MAGIC);
                    if magic == XFS_DIR3_LEAFN_MAGIC {
                        path.leaf = at;
                        return Ok(true);
                    }
                    let (_, below) = self.node_entries(at)?;
                    path.nodes.push((at, 0));
                    at = u64::from(below[0].1);
                }
                return Err(self.corrupt("its index is deeper than XFS allows"));
            }
        }
        Ok(false)
    }

    /// Where `name` is: its record, and its entry.
    fn find(&mut self, name: &[u8]) -> Result<Option<Found>> {
        let hash = hash_for(self.sb(), name);
        let mut path = self.descend(hash)?;
        loop {
            let records = self.leaf_records(path.leaf)?;
            for (index, &(h, address)) in records.iter().enumerate() {
                if h != hash || address == XFS_DIR2_NULL_DATAPTR {
                    continue;
                }
                let (db, offset) = self.split_address(address);
                let entries = self.data_entries(db)?;
                if entries.iter().any(|(o, e)| *o == offset && e.name == name) {
                    return Ok(Some(Found {
                        path,
                        index,
                        db,
                        offset,
                    }));
                }
            }
            // Names of one hash can run on into the next leaf.
            if records.last().map(|r| r.0) != Some(hash) || !self.next_leaf(&mut path)? {
                return Ok(None);
            }
        }
    }

    fn split_address(&self, address: u32) -> (u64, usize) {
        let byte = u64::from(address) * XFS_DIR2_DATA_ALIGN as u64;
        (byte / self.dbs as u64, (byte % self.dbs as u64) as usize)
    }

    /// Set the hash each node above keeps for the child on `path`'s way
    /// down, for as far up as that child was the last.
    fn fix_hashes(&mut self, nodes: &[(u64, usize)], mut hash: u32) -> Result<()> {
        for &(node, i) in nodes.iter().rev() {
            let (level, mut entries) = self.node_entries(node)?;
            if entries[i].0 == hash {
                return Ok(());
            }
            entries[i].0 = hash;
            let (back, forw) = self.siblings(node)?;
            self.write_node(node, back, forw, level, &entries);
            if i + 1 != entries.len() {
                return Ok(());
            }
            hash = entries[i].0;
        }
        Ok(())
    }

    /// Put a record in the index, splitting what overflows.
    fn index_insert(&mut self, allocations: &mut Allocations<'f>, record: Record) -> Result<()> {
        let path = self.descend(record.0)?;
        let mut records: Vec<Record> = self
            .leaf_records(path.leaf)?
            .into_iter()
            .filter(|r| r.1 != XFS_DIR2_NULL_DATAPTR)
            .collect();
        let at = records.partition_point(|r| r.0 <= record.0);
        records.insert(at, record);
        let (back, forw) = self.siblings(path.leaf)?;
        if records.len() <= self.index_capacity() {
            self.write_leaf(path.leaf, back, forw, &records);
            let last = records.last().expect("one was just put in").0;
            return self.fix_hashes(&path.nodes, last);
        }
        let right = records.split_off(records.len() / 2);
        let left_hash = records.last().expect("half").0;
        let right_hash = right.last().expect("half").0;
        if path.nodes.is_empty() {
            // The root is the one leaf: both halves move down a level.
            let a = self.new_index_block(allocations)?;
            let b = self.new_index_block(allocations)?;
            self.write_leaf(a, 0, b as u32, &records);
            self.write_leaf(b, a as u32, 0, &right);
            let root = self.leaf_da();
            self.write_node(
                root,
                0,
                0,
                1,
                &[(left_hash, a as u32), (right_hash, b as u32)],
            );
            return Ok(());
        }
        let new = self.new_index_block(allocations)?;
        self.write_leaf(path.leaf, back, new as u32, &records);
        self.write_leaf(new, path.leaf as u32, forw, &right);
        if forw != 0 {
            self.set_back(u64::from(forw), new as u32)?;
        }
        self.node_insert(allocations, &path.nodes, left_hash, new, right_hash)
    }

    /// The child on `nodes`' last step split: it now ends at `left_hash`,
    /// and `new` after it ends at `right_hash`.
    fn node_insert(
        &mut self,
        allocations: &mut Allocations<'f>,
        nodes: &[(u64, usize)],
        left_hash: u32,
        new: u64,
        right_hash: u32,
    ) -> Result<()> {
        let (&(node, i), above) = nodes.split_last().expect("a node");
        let (level, mut entries) = self.node_entries(node)?;
        entries[i].0 = left_hash;
        entries.insert(i + 1, (right_hash, new as u32));
        let (back, forw) = self.siblings(node)?;
        if entries.len() <= self.index_capacity() {
            self.write_node(node, back, forw, level, &entries);
            let last = entries.last().expect("not empty").0;
            return self.fix_hashes(above, last);
        }
        let right = entries.split_off(entries.len() / 2);
        let l = entries.last().expect("half").0;
        let r = right.last().expect("half").0;
        if above.is_empty() {
            if level + 1 >= XFS_DA_NODE_MAXDEPTH {
                return Err(self.corrupt("its index would grow deeper than XFS allows"));
            }
            let a = self.new_index_block(allocations)?;
            let b = self.new_index_block(allocations)?;
            self.write_node(a, 0, b as u32, level, &entries);
            self.write_node(b, a as u32, 0, level, &right);
            self.write_node(node, 0, 0, level + 1, &[(l, a as u32), (r, b as u32)]);
            return Ok(());
        }
        let sibling = self.new_index_block(allocations)?;
        self.write_node(node, back, sibling as u32, level, &entries);
        self.write_node(sibling, node as u32, forw, level, &right);
        if forw != 0 {
            self.set_back(u64::from(forw), sibling as u32)?;
        }
        self.node_insert(allocations, above, l, sibling, r)
    }

    /// Take a block out of its level's sibling chain.
    fn unlink(&mut self, dablk: u64) -> Result<()> {
        let (back, forw) = self.siblings(dablk)?;
        if back != 0 {
            self.set_forw(u64::from(back), forw)?;
        }
        if forw != 0 {
            self.set_back(u64::from(forw), back)?;
        }
        Ok(())
    }

    /// Take record `index` out of the leaf `path` ends at.
    fn index_remove(
        &mut self,
        allocations: &mut Allocations<'f>,
        path: &Path,
        index: usize,
    ) -> Result<()> {
        let mut records = self.leaf_records(path.leaf)?;
        records.remove(index);
        records.retain(|r| r.1 != XFS_DIR2_NULL_DATAPTR);
        if !records.is_empty() || path.nodes.is_empty() {
            let (back, forw) = self.siblings(path.leaf)?;
            self.write_leaf(path.leaf, back, forw, &records);
            if let Some(&(last, _)) = records.last() {
                self.fix_hashes(&path.nodes, last)?;
            }
            self.join(allocations, &path.nodes)?;
            return self.collapse_root(allocations);
        }
        self.unlink(path.leaf)?;
        self.release(allocations, path.leaf)?;
        self.node_remove(allocations, &path.nodes)?;
        self.collapse_root(allocations)
    }

    /// The child on `nodes`' last step is gone.
    fn node_remove(
        &mut self,
        allocations: &mut Allocations<'f>,
        nodes: &[(u64, usize)],
    ) -> Result<()> {
        let (&(node, i), above) = nodes.split_last().expect("a node");
        let (level, mut entries) = self.node_entries(node)?;
        entries.remove(i);
        if entries.is_empty() {
            if above.is_empty() {
                return Err(self.corrupt("its index would be left with no records"));
            }
            self.unlink(node)?;
            self.release(allocations, node)?;
            return self.node_remove(allocations, above);
        }
        let (back, forw) = self.siblings(node)?;
        self.write_node(node, back, forw, level, &entries);
        if i == entries.len() {
            let last = entries.last().expect("not empty").0;
            self.fix_hashes(above, last)?;
        }
        self.join(allocations, above)
    }

    /// The live records of a leaf, or the entries of a node.
    fn contents(&mut self, dablk: u64, leaf: bool) -> Result<Vec<Record>> {
        if leaf {
            Ok(self
                .leaf_records(dablk)?
                .into_iter()
                .filter(|r| r.1 != XFS_DIR2_NULL_DATAPTR)
                .collect())
        } else {
            Ok(self.node_entries(dablk)?.1)
        }
    }

    /// The child on `nodes`' last step, if it is down to three eighths of
    /// a block, joins a neighbour under the same parent when the two fit
    /// three quarters of one: the kernel's thresholds, so a directory that
    /// empties folds its index back up rather than keeping a tree of
    /// near-empty blocks. The parent may then be small enough to join its
    /// own neighbour.
    ///
    /// The kernel will also join across parents; this does not, which
    /// leaves a valid tree a little emptier than the kernel's would be.
    fn join(&mut self, allocations: &mut Allocations<'f>, nodes: &[(u64, usize)]) -> Result<()> {
        let Some((&(parent, i), above)) = nodes.split_last() else {
            return Ok(());
        };
        let cap = self.index_capacity();
        let (level, mut entries) = self.node_entries(parent)?;
        let leaf = level == 1;
        let child = u64::from(entries[i].1);
        if self.contents(child, leaf)?.len() * 8 > cap * 3 {
            return Ok(());
        }
        let (li, ri) = if i + 1 < entries.len() {
            (i, i + 1)
        } else if i > 0 {
            (i - 1, i)
        } else {
            return Ok(());
        };
        let (l, r) = (u64::from(entries[li].1), u64::from(entries[ri].1));
        let mut merged = self.contents(l, leaf)?;
        merged.extend(self.contents(r, leaf)?);
        if merged.len() * 4 > cap * 3 {
            return Ok(());
        }
        let (back, _) = self.siblings(l)?;
        let (_, forw) = self.siblings(r)?;
        if leaf {
            self.write_leaf(l, back, forw, &merged);
        } else {
            self.write_node(l, back, forw, level - 1, &merged);
        }
        if forw != 0 {
            self.set_back(u64::from(forw), l as u32)?;
        }
        self.release(allocations, r)?;
        entries[li].0 = merged.last().expect("two blocks, not empty").0;
        entries.remove(ri);
        let (pback, pforw) = self.siblings(parent)?;
        self.write_node(parent, pback, pforw, level, &entries);
        if li + 1 == entries.len() {
            self.fix_hashes(above, entries[li].0)?;
        }
        self.join(allocations, above)
    }

    /// A root node with one child gives way to that child, as many times
    /// as that is so (`xfs_da3_root_join`).
    fn collapse_root(&mut self, allocations: &mut Allocations<'f>) -> Result<()> {
        let root = self.leaf_da();
        loop {
            if be16(self.get(root)?, offsets::da_blk::MAGIC) != XFS_DA3_NODE_MAGIC {
                return Ok(());
            }
            let (_, entries) = self.node_entries(root)?;
            if entries.len() != 1 {
                return Ok(());
            }
            let child = u64::from(entries[0].1);
            let mut moved = self.get(child)?.clone();
            let blkno = crate::alloc_btree::blkno_of_fsbno(self.sb(), self.fsblock(root));
            moved[offsets::da_blk::BLKNO..offsets::da_blk::BLKNO + 8]
                .copy_from_slice(&blkno.to_be_bytes());
            moved[offsets::da_blk::FORW..offsets::da_blk::BACK + 4].fill(0);
            self.put(root, moved);
            self.release(allocations, child)?;
        }
    }

    // ------------------------------------------------------------------
    // Data blocks and the free index
    // ------------------------------------------------------------------

    /// Set the best of data block `db` in the free index, or mark it a
    /// hole, adding or giving back the free block that keeps it.
    fn set_best(
        &mut self,
        allocations: &mut Allocations<'f>,
        db: u64,
        best: Option<u16>,
    ) -> Result<()> {
        let max = self.bests_per_block() as u64;
        let k = db / max;
        let dablk = self.free_da() + k * self.per;
        let firstdb = k * max;
        let (mut bests, mut nused) = if self.map.contains_key(&dablk) {
            let b = self.get(dablk)?;
            let first = u64::from(be32(b, offsets::free_hdr::firstdb(true)));
            let nvalid = be32(b, offsets::free_hdr::nvalid(true)) as usize;
            let nused = be32(b, offsets::free_hdr::nused(true));
            if be32(b, 0) != XFS_DIR3_FREE_MAGIC || first != firstdb || nvalid > max as usize {
                return Err(self.corrupt(format!("free block {dablk} does not describe {db}")));
            }
            let bests: Vec<u16> = (0..nvalid)
                .map(|i| be16(b, offsets::free_hdr::bests(true, i)))
                .collect();
            (bests, nused)
        } else if best.is_none() {
            return Ok(());
        } else {
            self.add(allocations, dablk)?;
            (Vec::new(), 0)
        };
        let i = (db - firstdb) as usize;
        let old = bests.get(i).copied().unwrap_or(XFS_DIR2_DATA_FREE_NULL);
        match best {
            Some(v) => {
                if i >= bests.len() {
                    bests.resize(i + 1, XFS_DIR2_DATA_FREE_NULL);
                }
                bests[i] = v;
                if old == XFS_DIR2_DATA_FREE_NULL {
                    nused += 1;
                }
            }
            None => {
                if i < bests.len() {
                    bests[i] = XFS_DIR2_DATA_FREE_NULL;
                    if old != XFS_DIR2_DATA_FREE_NULL {
                        nused -= 1;
                    }
                    while bests.last() == Some(&XFS_DIR2_DATA_FREE_NULL) {
                        bests.pop();
                    }
                }
            }
        }
        if nused == 0 {
            return self.release(allocations, dablk);
        }
        let block = free_block(
            self.sb(),
            self.fsblock(dablk),
            self.ino,
            self.dbs,
            firstdb,
            nused,
            &bests,
        );
        self.put(dablk, block);
        Ok(())
    }

    /// The first data block whose longest free region holds `size` bytes.
    fn room_for(&mut self, size: usize) -> Result<Option<u64>> {
        let frees: Vec<u64> = self.map.range(self.free_da()..).map(|(&d, _)| d).collect();
        let max = self.bests_per_block();
        for dablk in frees {
            let b = self.get(dablk)?;
            let first = u64::from(be32(b, offsets::free_hdr::firstdb(true)));
            let nvalid = be32(b, offsets::free_hdr::nvalid(true)) as usize;
            for i in 0..nvalid.min(max) {
                let best = be16(b, offsets::free_hdr::bests(true, i));
                if best != XFS_DIR2_DATA_FREE_NULL && usize::from(best) >= size {
                    return Ok(Some(first + i as u64));
                }
            }
        }
        Ok(None)
    }

    /// A new, empty data block in the first hole of the data space.
    fn new_data_block(&mut self, allocations: &mut Allocations<'f>) -> Result<u64> {
        let mut db = 0;
        while self.map.contains_key(&(db * self.per)) {
            db += 1;
        }
        if db * self.per >= self.leaf_da() {
            return Err(self.corrupt("its data space is full"));
        }
        let fsblock = self.add(allocations, db * self.per)?;
        let (block, _) = data_block(self.sb(), fsblock, self.ino, self.dbs, &[])?;
        self.put(db * self.per, block);
        self.size = self.size.max((db + 1) * self.dbs as u64);
        Ok(db)
    }

    fn add_entry(&mut self, allocations: &mut Allocations<'f>, entry: &Entry) -> Result<()> {
        if self.find(&entry.name)?.is_some() {
            return Err(Error::AlreadyExists);
        }
        let size = entry_size(entry.name.len());
        let db = match self.room_for(size)? {
            Some(db) => db,
            None => self.new_data_block(allocations)?,
        };
        let mut entries = self.data_entries(db)?;
        let offset = first_gap(&entries, self.dbs, size)
            .ok_or_else(|| self.corrupt(format!("data block {db}'s best does not fit a name")))?;
        entries.push((offset, entry.clone()));
        let fsblock = self.fsblock(db * self.per);
        let (block, best) = data_block(self.sb(), fsblock, self.ino, self.dbs, &entries)?;
        self.put(db * self.per, block);
        self.set_best(allocations, db, Some(best))?;
        let address = (db * self.dbs as u64 + offset as u64) / XFS_DIR2_DATA_ALIGN as u64;
        let address = u32::try_from(address)
            .map_err(|_| self.corrupt("a data block lies past what an index record holds"))?;
        self.index_insert(allocations, (hash_for(self.sb(), &entry.name), address))
    }

    fn remove_entry(&mut self, allocations: &mut Allocations<'f>, name: &[u8]) -> Result<()> {
        let found = self.find(name)?.ok_or(Error::NotFound)?;
        self.index_remove(allocations, &found.path, found.index)?;
        let mut entries = self.data_entries(found.db)?;
        entries.retain(|(o, _)| *o != found.offset);
        if entries.is_empty() && found.db != 0 {
            self.release(allocations, found.db * self.per)?;
            self.set_best(allocations, found.db, None)?;
            if (found.db + 1) * self.dbs as u64 == self.size {
                self.size = found.db * self.dbs as u64;
            }
            return Ok(());
        }
        let fsblock = self.fsblock(found.db * self.per);
        let (block, best) = data_block(self.sb(), fsblock, self.ino, self.dbs, &entries)?;
        self.put(found.db * self.per, block);
        self.set_best(allocations, found.db, Some(best))
    }

    fn reparent(&mut self, parent: u64) -> Result<()> {
        let mut entries = self.data_entries(0)?;
        let dotdot = entries
            .iter_mut()
            .find(|(_, e)| e.name == b"..")
            .ok_or_else(|| self.corrupt("its first data block has no `..`"))?;
        dotdot.1.ino = parent;
        let (block, _) = data_block(self.sb(), self.fsblock(0), self.ino, self.dbs, &entries)?;
        self.put(0, block);
        Ok(())
    }

    /// Apply `edits` to a node-form directory, in place.
    pub(crate) fn edit(
        &mut self,
        allocations: &mut Allocations<'f>,
        edits: &[DirEdit<'_>],
    ) -> Result<()> {
        for edit in edits {
            match edit {
                DirEdit::Remove(name) => self.remove_entry(allocations, name)?,
                DirEdit::Add(entry) => self.add_entry(allocations, entry)?,
                DirEdit::Reparent(parent) => self.reparent(*parent)?,
                DirEdit::Rename(from, to) => {
                    if self.find(to)?.is_some() {
                        return Err(Error::AlreadyExists);
                    }
                    let found = self.find(from)?.ok_or(Error::NotFound)?;
                    let mut moved = self
                        .data_entries(found.db)?
                        .into_iter()
                        .find(|(o, _)| *o == found.offset)
                        .map(|(_, e)| e)
                        .ok_or_else(|| self.corrupt("an index record names no entry"))?;
                    self.remove_entry(allocations, from)?;
                    moved.name = to.to_vec();
                    self.add_entry(allocations, &moved)?;
                }
            }
        }
        Ok(())
    }

    /// The entries, when the directory is down to one leaf and they would
    /// fit leaf form again.
    pub(crate) fn fits_leaf_form(&mut self) -> Result<Option<Vec<Entry>>> {
        let root = self.leaf_da();
        if be16(self.get(root)?, offsets::da_blk::MAGIC) != XFS_DIR3_LEAFN_MAGIC {
            return Ok(None);
        }
        let entries = self.entries()?;
        let groups = crate::dir_leaf::pack(&entries, self.dbs)?.len();
        Ok((entries.len() <= crate::dir_leaf::leaf_capacity(self.dbs, groups)).then_some(entries))
    }

    // ------------------------------------------------------------------
    // Laying a directory out whole
    // ------------------------------------------------------------------

    /// Lay the directory out again holding exactly `entries`, in block,
    /// leaf or node form, whichever is the smallest that holds them.
    ///
    /// Blocks it keeps are checked and rewritten where they are; blocks it
    /// needs are taken; blocks it no longer needs go back.
    pub(crate) fn lay_out(
        &mut self,
        allocations: &mut Allocations<'f>,
        entries: &[Entry],
    ) -> Result<()> {
        let sb = self.sb();
        let dbs = self.dbs;
        let per = self.per;
        let leaf_da = self.leaf_da();
        let free_da = self.free_da();
        let block_form = dir_block::space_needed(entries) <= dbs;
        let groups = if block_form {
            vec![entries.to_vec()]
        } else {
            crate::dir_leaf::pack(entries, dbs)?
        };
        let leaf_form =
            !block_form && entries.len() <= crate::dir_leaf::leaf_capacity(dbs, groups.len());

        // Which blocks the new layout has.
        let mut wanted: Vec<u64> = (0..groups.len() as u64).map(|db| db * per).collect();
        let cap = self.index_capacity();
        let leaves = if block_form || leaf_form {
            Vec::new()
        } else {
            crate::bmbt_write::shares(entries.len(), cap)
        };
        // Index levels, bottom up, each a list of block sizes.
        let mut levels: Vec<Vec<usize>> = Vec::new();
        if !leaves.is_empty() {
            levels.push(leaves.clone());
            while levels.last().expect("one").len() > 1 {
                let below = levels.last().expect("one").len();
                levels.push(crate::bmbt_write::shares(below, cap));
                if levels.len() > usize::from(XFS_DA_NODE_MAXDEPTH) {
                    return Err(self.corrupt("its index would be deeper than XFS allows"));
                }
            }
        }
        let index_blocks: usize = levels.iter().map(Vec::len).sum();
        if leaf_form || index_blocks > 0 {
            wanted.push(leaf_da);
            for i in 1..index_blocks as u64 {
                wanted.push(leaf_da + i * per);
            }
        }
        let max = self.bests_per_block();
        if index_blocks > 0 {
            for k in 0..groups.len().div_ceil(max) as u64 {
                wanted.push(free_da + k * per);
            }
        }

        // Keep, take, give back.
        let had: Vec<u64> = self.map.keys().copied().collect();
        for dablk in had {
            if !wanted.contains(&dablk) {
                self.release(allocations, dablk)?;
            }
        }
        for &dablk in &wanted {
            if self.map.contains_key(&dablk) {
                self.get(dablk)?;
            } else {
                self.add(allocations, dablk)?;
            }
        }
        self.size = groups.len() as u64 * dbs as u64;

        if block_form {
            let block = dir_block::build(sb, self.fsblock(0), self.ino, entries)?;
            self.put(0, block);
            return Ok(());
        }
        let mut index = Vec::with_capacity(entries.len());
        let mut bests = Vec::with_capacity(groups.len());
        for (db, group) in groups.iter().enumerate() {
            let da = db as u64 * per;
            let built = crate::dir_leaf::build_data_block(
                sb,
                self.fsblock(da),
                self.ino,
                db as u64,
                group,
            )?;
            index.extend(built.index);
            bests.push(built.best);
            self.put(da, built.bytes);
        }
        if leaf_form {
            let block =
                crate::dir_leaf::build_leaf(sb, self.fsblock(leaf_da), self.ino, index, &bests)?;
            self.put(leaf_da, block);
            return Ok(());
        }

        // Node form: leaves, then nodes, the top one at the root.
        index.sort_by_key(|&(hash, _)| hash);
        let mut next = 1u64;
        let mut place = |top: bool| {
            if top {
                leaf_da
            } else {
                let at = leaf_da + next * per;
                next += 1;
                at
            }
        };
        let height = levels.len();
        // (dablk, last hash) of each block of the level just built.
        let mut below: Vec<(u64, u32)> = Vec::new();
        let mut at = 0;
        let leaf_blocks: Vec<u64> = (0..levels[0].len()).map(|_| place(height == 1)).collect();
        for (i, &n) in levels[0].iter().enumerate() {
            let back = if i == 0 { 0 } else { leaf_blocks[i - 1] as u32 };
            let forw = leaf_blocks.get(i + 1).map_or(0, |&d| d as u32);
            let records = &index[at..at + n];
            self.write_leaf(leaf_blocks[i], back, forw, records);
            below.push((leaf_blocks[i], records.last().expect("never empty").0));
            at += n;
        }
        for (depth, sizes) in levels.iter().enumerate().skip(1) {
            let top = depth + 1 == height;
            let blocks: Vec<u64> = (0..sizes.len()).map(|_| place(top)).collect();
            let mut above = Vec::new();
            let mut at = 0;
            for (i, &n) in sizes.iter().enumerate() {
                let back = if i == 0 { 0 } else { blocks[i - 1] as u32 };
                let forw = blocks.get(i + 1).map_or(0, |&d| d as u32);
                let children: Vec<Record> = below[at..at + n]
                    .iter()
                    .map(|&(d, h)| (h, d as u32))
                    .collect();
                self.write_node(blocks[i], back, forw, depth as u16, &children);
                above.push((blocks[i], children.last().expect("never empty").0));
                at += n;
            }
            below = above;
        }
        for (k, chunk) in bests.chunks(max).enumerate() {
            let dablk = free_da + k as u64 * per;
            let block = free_block(
                sb,
                self.fsblock(dablk),
                self.ino,
                dbs,
                (k * max) as u64,
                chunk.len() as u32,
                chunk,
            );
            self.put(dablk, block);
        }
        Ok(())
    }

    /// Everything the edit changed, logged, with the fork that maps it.
    pub(crate) fn finish(
        self,
        allocations: &mut Allocations<'f>,
        dir: &Inode,
        raw: &[u8],
    ) -> Result<Rewritten> {
        let sb = self.sb();
        let mut items = Vec::new();
        for c in self.cache.into_values() {
            if c.after == c.before {
                continue;
            }
            let kind = kind_of(&c.after).ok_or_else(|| {
                Error::Internal(format!(
                    "inode {}'s directory block at {} was left without a magic",
                    self.ino, c.fsblock
                ))
            })?;
            let blkno = crate::alloc_btree::blkno_of_fsbno(sb, c.fsblock);
            items.push(changed_chunks(blkno, &c.before, c.after, kind));
        }
        let mut extents: Vec<Extent> = Vec::new();
        for (&dablk, &fsblock) in &self.map {
            match extents.last_mut() {
                Some(e)
                    if e.startoff + e.blockcount == dablk
                        && e.startblock + e.blockcount == fsblock =>
                {
                    e.blockcount += self.per;
                }
                _ => extents.push(Extent {
                    startoff: dablk,
                    startblock: fsblock,
                    blockcount: self.per,
                    unwritten: false,
                }),
            }
        }
        let fork = self
            .fs
            .map_data_fork(allocations, self.ino, dir, raw, &extents)?;
        items.extend(fork.items);
        Ok(Rewritten {
            items,
            fork: fork.fork,
            logged_fork: fork.logged,
            fields: fork.fields,
            format: fork.format,
            size: self.size,
            blocks: self.map.len() as u64 * self.per + fork.tree_blocks,
            nextents: extents.len() as u64,
        })
    }
}

/// The buffer type a directory block is logged as, from its magic.
fn kind_of(block: &[u8]) -> Option<u16> {
    match be32(block, 0) {
        XFS_DIR3_BLOCK_MAGIC => return Some(BLFT_DIR_BLOCK),
        XFS_DIR3_DATA_MAGIC => return Some(BLFT_DIR_DATA),
        XFS_DIR3_FREE_MAGIC => return Some(BLFT_DIR_FREE),
        _ => {}
    }
    match be16(block, offsets::da_blk::MAGIC) {
        XFS_DIR3_LEAF1_MAGIC => Some(BLFT_DIR_LEAF1),
        XFS_DIR3_LEAFN_MAGIC => Some(BLFT_DIR_LEAFN),
        XFS_DA3_NODE_MAGIC => Some(BLFT_DA_NODE),
        _ => None,
    }
}

/// A leafn or node block: its header, its siblings, and its records.
#[allow(clippy::too_many_arguments)]
fn index_block(
    sb: &Superblock,
    fsblock: u64,
    owner: u64,
    dbs: usize,
    magic: u16,
    (back, forw): (u32, u32),
    level: Option<u16>,
    records: &[Record],
) -> Vec<u8> {
    use offsets::da_blk as b;
    let mut block = vec![0u8; dbs];
    block[b::FORW..b::FORW + 4].copy_from_slice(&forw.to_be_bytes());
    block[b::BACK..b::BACK + 4].copy_from_slice(&back.to_be_bytes());
    block[b::MAGIC..b::MAGIC + 2].copy_from_slice(&magic.to_be_bytes());
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[b::BLKNO..b::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[b::UUID..b::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[b::OWNER..b::OWNER + 8].copy_from_slice(&owner.to_be_bytes());
    let count = records.len() as u16;
    let at = offsets::leaf_hdr::count(true);
    block[at..at + 2].copy_from_slice(&count.to_be_bytes());
    if let Some(level) = level {
        let at = offsets::node_hdr::level(true);
        block[at..at + 2].copy_from_slice(&level.to_be_bytes());
    }
    for (i, &(hash, value)) in records.iter().enumerate() {
        let at = DA_HDR + i * RECORD;
        block[at..at + 4].copy_from_slice(&hash.to_be_bytes());
        block[at + 4..at + 8].copy_from_slice(&value.to_be_bytes());
    }
    block
}

/// A free block of the bests of `firstdb` onwards.
fn free_block(
    sb: &Superblock,
    fsblock: u64,
    owner: u64,
    dbs: usize,
    firstdb: u64,
    nused: u32,
    bests: &[u16],
) -> Vec<u8> {
    use offsets::dir3_blk as h;
    let mut block = vec![0u8; dbs];
    block[h::MAGIC..h::MAGIC + 4].copy_from_slice(&XFS_DIR3_FREE_MAGIC.to_be_bytes());
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[h::BLKNO..h::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[h::UUID..h::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[h::OWNER..h::OWNER + 8].copy_from_slice(&owner.to_be_bytes());
    let put = |block: &mut Vec<u8>, at: usize, v: u32| {
        block[at..at + 4].copy_from_slice(&v.to_be_bytes());
    };
    put(&mut block, offsets::free_hdr::firstdb(true), firstdb as u32);
    put(
        &mut block,
        offsets::free_hdr::nvalid(true),
        bests.len() as u32,
    );
    put(&mut block, offsets::free_hdr::nused(true), nused);
    for (i, best) in bests.iter().enumerate() {
        let at = XFS_DIR3_FREE_HDR_SIZE + i * BEST;
        block[at..at + 2].copy_from_slice(&best.to_be_bytes());
    }
    block
}

/// The first gap of at least `size` bytes between a data block's entries.
fn first_gap(entries: &[(usize, Entry)], dbs: usize, size: usize) -> Option<usize> {
    let mut placed: Vec<(usize, usize)> = entries
        .iter()
        .map(|(o, e)| (*o, entry_size(e.name.len())))
        .collect();
    placed.sort_unstable();
    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    for (o, len) in placed {
        if o >= at + size {
            return Some(at);
        }
        at = at.max(o + len);
    }
    (dbs >= at + size).then_some(at)
}

/// A data block holding `entries` each at its own offset, with the gaps
/// between them as free regions; and its longest free region.
///
/// Entries keep their offsets because their index records hold them: a
/// name added or removed changes its own block's free space and nothing
/// else's address.
fn data_block(
    sb: &Superblock,
    fsblock: u64,
    owner: u64,
    dbs: usize,
    entries: &[(usize, Entry)],
) -> Result<(Vec<u8>, u16)> {
    use offsets::dir3_blk as h;
    let mut block = vec![0u8; dbs];
    block[h::MAGIC..h::MAGIC + 4].copy_from_slice(&XFS_DIR3_DATA_MAGIC.to_be_bytes());
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[h::BLKNO..h::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[h::UUID..h::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[h::OWNER..h::OWNER + 8].copy_from_slice(&owner.to_be_bytes());

    let mut sorted: Vec<&(usize, Entry)> = entries.iter().collect();
    sorted.sort_by_key(|(o, _)| *o);
    let mut free: Vec<(usize, usize)> = Vec::new();
    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    for (offset, e) in sorted {
        let (offset, len) = (*offset, entry_size(e.name.len()));
        if offset < at || offset + len > dbs || offset % XFS_DIR2_DATA_ALIGN != 0 {
            return Err(Error::Internal(format!(
                "inode {owner}'s entry at {offset} overlaps another or the block's end"
            )));
        }
        if offset > at {
            free.push((at, offset - at));
        }
        use offsets::data_entry as d;
        let namelen = e.name.len();
        block[offset + d::INUMBER..offset + d::INUMBER + 8].copy_from_slice(&e.ino.to_be_bytes());
        block[offset + d::NAMELEN] = namelen as u8;
        block[offset + d::NAME..offset + d::NAME + namelen].copy_from_slice(&e.name);
        block[offset + d::NAME + namelen] = e.ftype;
        block[offset + len - 2..offset + len].copy_from_slice(&(offset as u16).to_be_bytes());
        at = offset + len;
    }
    if at < dbs {
        free.push((at, dbs - at));
    }
    for &(offset, len) in &free {
        use offsets::data_unused as u;
        block[offset + u::FREETAG..offset + u::FREETAG + 2]
            .copy_from_slice(&XFS_DIR2_DATA_FREE_TAG.to_be_bytes());
        block[offset + u::LENGTH..offset + u::LENGTH + 2]
            .copy_from_slice(&(len as u16).to_be_bytes());
        let tag = offset + u::tag(len);
        block[tag..tag + 2].copy_from_slice(&(offset as u16).to_be_bytes());
    }
    // The three longest, longest first.
    free.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (i, &(offset, len)) in free.iter().take(3).enumerate() {
        let bf = offsets::data_hdr::bestfree(offsets::data_hdr::V5_BESTFREE, i);
        block[bf..bf + 2].copy_from_slice(&(offset as u16).to_be_bytes());
        block[bf + 2..bf + 4].copy_from_slice(&(len as u16).to_be_bytes());
    }
    let best = free.first().map_or(0, |&(_, len)| len as u16);
    Ok((block, best))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(name: &str) -> Entry {
        Entry {
            name: name.as_bytes().to_vec(),
            ino: 128,
            ftype: 1,
        }
    }

    #[test]
    fn a_gap_is_found_between_entries_and_after_them() {
        let a = entry_size(1);
        let entries = vec![(64, e("a")), (64 + 2 * a, e("b"))];
        assert_eq!(first_gap(&entries, 1024, a), Some(64 + a));
        assert_eq!(first_gap(&entries, 1024, 2 * a), Some(64 + 3 * a));
        assert_eq!(first_gap(&entries, 64 + 3 * a, 2 * a), None);
    }
}
