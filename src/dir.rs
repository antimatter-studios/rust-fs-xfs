//! XFS directory formats: short form, block, leaf and node.
//!
//! A directory is stored in one of four shapes, and which one is in use
//! is a property of the inode rather than of the directory:
//!
//! | Shape | Where it lives | Recognised by |
//! |---|---|---|
//! | **short form** | inline in the inode's data fork | `di_format == Local` |
//! | **block** | one directory block holding data *and* index | data magic `XDB3` / `XD2B` |
//! | **leaf** | data blocks plus one index block | data `XDD3` / `XD2D`, index `0x3df1` / `0xd2f1` |
//! | **node** | as leaf, but the index is a B-tree | node magic `0x3ebe` / `0xfebe` |
//!
//! This module turns bytes into entries and nothing more. Finding which
//! block holds a given directory offset is the extent map's job, and
//! walking a path is the filesystem's; neither happens here.
//!
//! # Byte order
//!
//! Big-endian throughout, as everywhere in XFS, with the standing
//! exception that CRC fields are little-endian. See [`crate::superblock`].
//!
//! # `.` and `..`
//!
//! The two shapes do not agree on this and callers must not assume.
//! A short-form directory stores neither: `..` is the header's parent
//! inode ([`ShortFormDir::parent_ino`]) and `.` is the directory's own
//! inode number. Every other shape stores both as ordinary entries, so
//! [`parse_data_block`] returns them like any other name.
//!
//! # Self-describing blocks
//!
//! On v5 every directory block carries a CRC32C, the filesystem UUID,
//! the disk address it believes it lives at, and the inode number that
//! owns it. [`verify_data_block`] and [`verify_da_block`] check all four,
//! for the reason spelled out in [`crate::ag`]: the checksum catches
//! corrupted bits, the identity fields catch an intact block that came
//! from the wrong place.

use crate::endian::{be16, be32, be64, le32, uuid_at};
use crate::error::{Error, Result};
use crate::inode::{FileType, Format, Inode};
use crate::superblock::{crc32c_with_zeroed_crc, version_flags, Superblock};

/// The directory and B-tree-node layout constants this module parses
/// with. They are defined once, in [`crate::format::dir`], alongside the
/// rest of the format — including the structures nothing here reads yet
/// — so that a value can be checked against its neighbours rather than
/// against a second copy that happens to agree.
pub use crate::format::dir::{
    DATA_ENTRY_MIN_SIZE, XFS_DA3_NODE_HDR_SIZE, XFS_DA3_NODE_MAGIC, XFS_DA_NODE_ENTRY_SIZE,
    XFS_DA_NODE_HDR_SIZE, XFS_DA_NODE_MAGIC, XFS_DA_NODE_MAXDEPTH, XFS_DIR2_BLOCK_MAGIC,
    XFS_DIR2_BLOCK_TAIL_SIZE, XFS_DIR2_DATA_ALIGN, XFS_DIR2_DATA_FREE_TAG, XFS_DIR2_DATA_HDR_SIZE,
    XFS_DIR2_DATA_MAGIC, XFS_DIR2_FREE_MAGIC, XFS_DIR2_LEAF1_MAGIC, XFS_DIR2_LEAFN_MAGIC,
    XFS_DIR2_LEAF_ENTRY_SIZE, XFS_DIR2_LEAF_HDR_SIZE, XFS_DIR2_LEAF_TAIL_SIZE,
    XFS_DIR3_BLOCK_MAGIC, XFS_DIR3_DATA_HDR_SIZE, XFS_DIR3_DATA_MAGIC, XFS_DIR3_FREE_MAGIC,
    XFS_DIR3_LEAF1_MAGIC, XFS_DIR3_LEAFN_MAGIC, XFS_DIR3_LEAF_HDR_SIZE,
};

/// An index entry whose address is this has been removed but not yet
/// compacted away (`XFS_DIR2_NULL_DATAPTR`).
const NULL_DATAPTR: u32 = 0;

/// Largest inode number a short-form directory can hold in four bytes
/// (`XFS_DIR2_MAX_SHORT_INUM`). Anything above it forces the whole
/// directory to the 8-byte representation.
const MAX_SHORT_INUM: u64 = 0xffff_ffff;

/// XFS inode numbers are capped at 56 bits (`XFS_MAXINUMBER`), so the
/// top byte of an 8-byte inode number on disk is always zero.
const MAX_INUMBER: u64 = (1u64 << 56) - 1;

/// Deepest index this driver will descend. One interior level indexes
/// roughly a million names and two indexes far more than any directory
/// this driver has been exercised against, so a deeper tree is refused
/// outright rather than walked on the strength of an untested guess.
/// See [`parse_node`].
pub const MAX_SUPPORTED_NODE_LEVEL: u16 = 2;

/// `XFS_SB_VERSION2_FTYPE` — on a v4 filesystem the file-type feature is
/// advertised here rather than in the v5 incompatible feature mask.
const SB_VERSION2_FTYPE: u32 = 0x0000_0200;

/// Byte offsets within the on-disk directory structures.
///
/// Named for the same reason the superblock's and the AG headers' are.
/// Directories are the worst case for unnamed literals: there are six
/// distinct on-disk structures here, three of them come in a v4 and a v5
/// shape that differ only by a header prefix, and two of them place
/// different fields at the same offset.
pub mod offsets {
    /// `xfs_dir3_blk_hdr` — the v5 self-describing prefix on every data,
    /// block-form and free-index directory block.
    pub mod dir3_blk {
        /// Structure magic.
        pub const MAGIC: usize = 0;
        /// CRC32C, stored little-endian.
        pub const CRC: usize = 4;
        /// Disk address of the block, in 512-byte units.
        pub const BLKNO: usize = 8;
        /// Log sequence number of the last write.
        pub const LSN: usize = 16;
        /// Owning filesystem.
        pub const UUID: usize = 24;
        /// Inode number of the directory this block belongs to.
        pub const OWNER: usize = 40;
    }

    /// `xfs_da_blkinfo` and its v5 extension `xfs_da3_blkinfo` — the
    /// prefix on every leaf and B-tree node block. The first twelve
    /// bytes are common to both versions.
    pub mod da_blk {
        /// Next block in the chain at this level.
        pub const FORW: usize = 0;
        /// Previous block in the chain at this level.
        pub const BACK: usize = 4;
        /// Structure magic, a `u16` here rather than the `u32` the data
        /// blocks use.
        pub const MAGIC: usize = 8;
        /// CRC32C, stored little-endian. v5 only.
        pub const CRC: usize = 12;
        /// Disk address of the block, in 512-byte units. v5 only.
        pub const BLKNO: usize = 16;
        /// Log sequence number of the last write. v5 only.
        pub const LSN: usize = 24;
        /// Owning filesystem. v5 only.
        pub const UUID: usize = 32;
        /// Inode number of the directory this block belongs to. v5 only.
        pub const OWNER: usize = 48;
    }

    /// `xfs_dir2_sf_hdr` — the header of a short-form directory.
    pub mod sf_hdr {
        /// Number of entries, excluding `.` and `..`.
        pub const COUNT: usize = 0;
        /// How many inode numbers need more than 32 bits.
        pub const I8COUNT: usize = 1;
        /// Parent inode number, 4 or 8 bytes wide.
        pub const PARENT: usize = 2;
    }

    /// `xfs_dir2_sf_entry` — one entry of a short-form directory, packed
    /// with no alignment.
    pub mod sf_entry {
        /// Name length.
        pub const NAMELEN: usize = 0;
        /// Directory offset cookie.
        pub const OFFSET: usize = 1;
        /// The name itself, and then a file type byte and the inode
        /// number, both at name-length-dependent offsets.
        pub const NAME: usize = 3;
    }

    /// `xfs_dir2_data_entry` — one used entry in a directory data block.
    pub mod data_entry {
        /// Inode number.
        pub const INUMBER: usize = 0;
        /// Name length.
        pub const NAMELEN: usize = 8;
        /// The name itself, and then a file type byte and the trailing
        /// tag, both at name-length-dependent offsets.
        pub const NAME: usize = 9;
    }

    /// `xfs_dir2_data_unused` — one free region in a directory data
    /// block, told apart from a used entry by its leading tag.
    pub mod data_unused {
        /// `XFS_DIR2_DATA_FREE_TAG`, where a used entry has the top of
        /// its inode number.
        pub const FREETAG: usize = 0;
        /// Total length of the free region.
        pub const LENGTH: usize = 2;
    }

    /// `xfs_dir2_block_tail` — the last eight bytes of a block-form
    /// directory.
    pub mod block_tail {
        /// Number of hash index records.
        pub const COUNT: usize = 0;
        /// How many of them are stale.
        pub const STALE: usize = 4;
    }

    /// `xfs_dir2_leaf_entry` — one hash index record.
    pub mod leaf_entry {
        /// Hash of the entry's name.
        pub const HASHVAL: usize = 0;
        /// Address of the entry it points at.
        pub const ADDRESS: usize = 4;
    }

    /// `xfs_da_node_entry` — one child record of a B-tree node.
    pub mod node_entry {
        /// Highest hash value in the child's subtree.
        pub const HASHVAL: usize = 0;
        /// Directory-block number of the child.
        pub const BEFORE: usize = 4;
    }

    /// Offset of the count and stale/level pair that follows the block
    /// info header in a leaf or node block. The two structures put
    /// different fields in the same two slots: a leaf's second field is
    /// its stale count, a node's is its level.
    ///
    /// v5 headers end with four bytes of padding after the pair, v4
    /// headers have none, so the pair sits eight bytes before the end of
    /// a v5 header and four before the end of a v4 one.
    pub const fn da_counts(hdr_size: usize, is_v5: bool) -> usize {
        hdr_size - if is_v5 { 8 } else { 4 }
    }
}

/// Round `n` up to the directory entry alignment.
#[inline]
fn align_up(n: usize) -> usize {
    (n + XFS_DIR2_DATA_ALIGN - 1) & !(XFS_DIR2_DATA_ALIGN - 1)
}

/// Whether directory entries on this filesystem carry a file-type byte.
///
/// This is deliberately not [`Superblock::has_ftype`]. That method tests
/// only the v5 incompatible feature bit, which is where v5 filesystems
/// advertise the feature — but a **v4** filesystem advertises it in
/// `sb_features2` instead, and mkfs has enabled it by default there for
/// years. The `xfs-nocrc` oracle fixture is exactly that case: v4,
/// `features_incompat = 0`, `features2 = 0x28a`. Reading its directories
/// without the file-type byte shifts every inode number by one byte.
///
/// The two conditions together are what the kernel's
/// `xfs_sb_version_hasftype()` tests. Fixing [`Superblock::has_ftype`]
/// to match belongs in that module, not this one.
fn dir_has_ftype(sb: &Superblock) -> bool {
    if sb.has_ftype() {
        return true;
    }
    sb.versionnum & version_flags::MOREBITSBIT != 0 && sb.features2 & SB_VERSION2_FTYPE != 0
}

/// The file-type byte stored beside a directory entry name.
///
/// The encoding is the `XFS_DIR3_FT_*` table, which matches the
/// `DT_*` ordering used by `readdir`. `XFS_DIR3_FT_UNKNOWN` (0) and
/// `XFS_DIR3_FT_WHT` (8, an overlay whiteout) have no counterpart in
/// [`FileType`] and become `None` — as does a filesystem with no
/// file-type feature at all. In every one of those cases the caller has
/// to read the inode to learn the type.
///
/// # Errors
///
/// [`Error::BadSuperblock`] for a value at or above `XFS_DIR3_FT_MAX`,
/// which is the kernel's own bound and a good sign the entry was read at
/// the wrong offset.
pub fn ftype_from_raw(raw: u8) -> Result<Option<FileType>> {
    Ok(match raw {
        0 => None,
        1 => Some(FileType::Regular),
        2 => Some(FileType::Directory),
        3 => Some(FileType::CharDevice),
        4 => Some(FileType::BlockDevice),
        5 => Some(FileType::Fifo),
        6 => Some(FileType::Socket),
        7 => Some(FileType::Symlink),
        8 => None, // whiteout
        other => {
            return Err(Error::BadSuperblock(format!(
                "directory entry file type {other} is not a defined value"
            )))
        }
    })
}

/// One name in a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// The name, exactly as stored. XFS does not NUL-terminate names and
    /// does not require them to be valid UTF-8, so they stay as bytes.
    pub name: Vec<u8>,
    /// Inode number this name resolves to.
    pub ino: u64,
    /// File type, when the filesystem records one and it is a type this
    /// driver represents. `None` means the caller must read the inode.
    pub ftype: Option<FileType>,
    /// The entry's on-disk offset cookie: `xfs_dir2_sf_entry.offset` in
    /// short form, and the entry's byte offset within its own data block
    /// (the value XFS repeats in the entry's trailing tag) in every other
    /// shape. It is what a `readdir` cookie is built from; on its own it
    /// is not a filesystem-wide identifier.
    pub offset: u32,
}

/// A short-form directory: the whole thing, read out of an inode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShortFormDir {
    /// Inode number of the parent directory — the `..` entry, which
    /// short form stores in the header rather than as a name.
    pub parent_ino: u64,
    /// Number of inode numbers in this directory, parent included, that
    /// do not fit in 32 bits. When it is non-zero *every* inode number
    /// in the directory is stored in 8 bytes, not just the wide ones.
    pub i8count: u8,
    /// The entries, in on-disk order. Neither `.` nor `..` appears here.
    pub entries: Vec<DirEntry>,
}

/// One hash-index record: where a name whose hash is `hashval` lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafEntry {
    /// Hash of the entry's name. Records are sorted by this.
    pub hashval: u32,
    /// The entry's address, in 8-byte units from the start of the
    /// directory's data space. Zero marks a record whose name has been
    /// removed but which has not been compacted away yet.
    pub address: u32,
}

impl LeafEntry {
    /// Whether this record has been emptied but not yet compacted away.
    pub fn is_stale(&self) -> bool {
        self.address == NULL_DATAPTR
    }
}

/// A block-form directory: one directory block holding the entries and
/// the whole hash index together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockDir {
    /// Every name in the directory, `.` and `..` included.
    pub entries: Vec<DirEntry>,
    /// The hash index from the block's tail, sorted by hash.
    pub index: Vec<LeafEntry>,
}

/// A leaf block: the hash index of a leaf-form directory, or one leaf of
/// a node-form directory's B-tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirLeaf {
    /// The block's magic, which says which of the two it is.
    pub magic: u16,
    /// Next leaf block in the chain, or 0 for none.
    pub forw: u32,
    /// Previous leaf block in the chain, or 0 for none.
    pub back: u32,
    /// Number of index records.
    pub count: u16,
    /// How many of them are stale.
    pub stale: u16,
    /// The records, sorted by hash.
    pub entries: Vec<LeafEntry>,
    /// Length of the "best free" array in the block's tail. Only a
    /// leaf-form directory's single index block carries one; the leaves
    /// of a node-form directory do not.
    pub bestcount: Option<u32>,
}

impl DirLeaf {
    /// Whether this leaf is the sole index block of a leaf-form
    /// directory, as opposed to one leaf of a node-form B-tree.
    pub fn is_single_leaf(&self) -> bool {
        self.magic == XFS_DIR3_LEAF1_MAGIC || self.magic == XFS_DIR2_LEAF1_MAGIC
    }
}

/// One B-tree node record: everything hashing at or below `hashval`
/// lives in or under block `before`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeEntry {
    /// Highest hash value in the subtree rooted at `before`.
    pub hashval: u32,
    /// Directory-block number of the child.
    pub before: u32,
}

/// An interior node of a node-form directory's hash B-tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirNode {
    /// Height above the leaves. Leaves are level 0, so this is at
    /// least 1.
    pub level: u16,
    /// Number of child records.
    pub count: u16,
    /// Next node at this level, or 0 for none.
    pub forw: u32,
    /// Previous node at this level, or 0 for none.
    pub back: u32,
    /// The child records, sorted by hash.
    pub entries: Vec<NodeEntry>,
}

impl DirNode {
    /// The child to descend into when looking for `hash`: the first
    /// record whose hash bound is at or above it.
    ///
    /// Returns `None` when `hash` is above every bound in this node,
    /// which means the name is not in this subtree.
    ///
    /// This hands back a directory-block number and stops. Reading that
    /// block is the caller's business — this module does no I/O and owns
    /// no extent map.
    pub fn child_for_hash(&self, hash: u32) -> Option<u32> {
        self.entries
            .iter()
            .find(|e| e.hashval >= hash)
            .map(|e| e.before)
    }
}

/// Byte offsets of the identity fields inside a v5 block header. The two
/// families of directory block differ only in where they start.
struct V5Layout {
    crc: usize,
    blkno: usize,
    uuid: usize,
    owner: usize,
}

/// `xfs_dir3_blk_hdr`, fronting every data, block-form and free-index
/// directory block on v5.
const DIR3_BLK_HDR: V5Layout = V5Layout {
    crc: offsets::dir3_blk::CRC,
    blkno: offsets::dir3_blk::BLKNO,
    uuid: offsets::dir3_blk::UUID,
    owner: offsets::dir3_blk::OWNER,
};

/// `xfs_da3_blkinfo`, fronting every leaf and node block on v5.
const DA3_BLK_HDR: V5Layout = V5Layout {
    crc: offsets::da_blk::CRC,
    blkno: offsets::da_blk::BLKNO,
    uuid: offsets::da_blk::UUID,
    owner: offsets::da_blk::OWNER,
};

/// Directory block size, checked against the buffer we were handed.
///
/// Directories use their own block size, which is `sb_blocksize` shifted
/// by `sb_dirblklog` and so may be larger than a filesystem block.
fn dir_block_size(buf: &[u8], sb: &Superblock) -> Result<usize> {
    let size = sb.dirblocksize() as usize;
    if buf.len() < size {
        return Err(Error::BadSuperblock(format!(
            "directory block needs {size} bytes, got {}",
            buf.len()
        )));
    }
    Ok(size)
}

/// Verify a v5 self-describing block header.
///
/// `daddr` is the block's address **in 512-byte units**, which is what
/// `xfs_dir3_blk_hdr.blkno` records — a buffer disk address, not a
/// filesystem block number. Passing a filesystem block number here makes
/// every real directory block look misdirected.
///
/// On v4 there is no header to check and this is a no-op.
fn verify_v5_header(
    what: &'static str,
    layout: &V5Layout,
    buf: &[u8],
    sb: &Superblock,
    daddr: u64,
    owner: u64,
) -> Result<()> {
    if !sb.is_v5() {
        return Ok(());
    }
    // The checksum covers the whole directory block, not the header --
    // XFS hands its verifier the full buffer length. See AGENTS.md.
    let size = dir_block_size(buf, sb)?;
    // Checksums are little-endian; see `superblock::le32`.
    let stored = le32(buf, layout.crc);
    let computed = crc32c_with_zeroed_crc(&buf[..size], layout.crc);
    if stored != computed {
        return Err(Error::ChecksumMismatch { what, block: daddr });
    }
    if uuid_at(buf, layout.uuid) != sb.meta_uuid {
        return Err(Error::BlockIdentityMismatch {
            what,
            expected: daddr,
            found: u64::MAX, // UUID mismatch: the address is not meaningful
        });
    }
    let blkno = be64(buf, layout.blkno);
    if blkno != daddr {
        return Err(Error::BlockIdentityMismatch {
            what,
            expected: daddr,
            found: blkno,
        });
    }
    let found = be64(buf, layout.owner);
    if found != owner {
        return Err(Error::BlockIdentityMismatch {
            what: "directory block owner",
            expected: owner,
            found,
        });
    }
    Ok(())
}

/// Verify a v5 directory **data** block — the family fronted by
/// `xfs_dir3_blk_hdr`: data blocks (`XDD3`), block-form directories
/// (`XDB3`) and free-index blocks (`XDF3`).
///
/// `daddr` is the block's disk address in 512-byte units; `owner` is the
/// inode number of the directory it belongs to. A no-op on v4.
///
/// # Errors
///
/// [`Error::ChecksumMismatch`] on a bad CRC32C, and
/// [`Error::BlockIdentityMismatch`] when the UUID, the recorded address,
/// or the owning inode disagrees with what the caller expected.
pub fn verify_data_block(buf: &[u8], sb: &Superblock, daddr: u64, owner: u64) -> Result<()> {
    verify_v5_header("directory data block", &DIR3_BLK_HDR, buf, sb, daddr, owner)
}

/// Verify a v5 directory **index** block — the family fronted by
/// `xfs_da3_blkinfo`: leaf blocks and B-tree nodes.
///
/// Arguments and errors are as [`verify_data_block`].
pub fn verify_da_block(buf: &[u8], sb: &Superblock, daddr: u64, owner: u64) -> Result<()> {
    verify_v5_header("directory index block", &DA3_BLK_HDR, buf, sb, daddr, owner)
}

/// Size of one entry in a directory data block, padded to alignment:
/// inode number, name length, the name, an optional file type byte, and
/// the 2-byte tag repeating the entry's own offset.
fn data_entry_size(namelen: u8, ftype: bool) -> usize {
    align_up(offsets::data_entry::NAME + usize::from(namelen) + usize::from(ftype) + 2)
}

/// Reject an inode number a directory entry could not legitimately hold.
///
/// A zero inode number is never valid, and one naming an allocation
/// group past the end of the filesystem is the classic shape of a read
/// at the wrong offset. This is the cheap half of the kernel's
/// `xfs_dir_ino_validate()`; the expensive half needs the AG's inode
/// btree and is not available here.
fn check_entry_ino(sb: &Superblock, ino: u64, what: &str) -> Result<()> {
    if ino == 0 {
        return Err(Error::BadSuperblock(format!(
            "{what}: inode number is zero"
        )));
    }
    if ino > MAX_INUMBER {
        return Err(Error::BadSuperblock(format!(
            "{what}: inode number {ino} exceeds the 56-bit maximum"
        )));
    }
    let (ag, _, _) = sb.split_ino(ino);
    if ag >= sb.agcount {
        return Err(Error::BadSuperblock(format!(
            "{what}: inode number {ino} names allocation group {ag}, but there are only {}",
            sb.agcount
        )));
    }
    Ok(())
}

/// Walk the entries in `buf[start..end]`, which must be the used region
/// of one directory data block.
///
/// The region is a run of variable-length records, each either a used
/// entry or a free one. They are told apart by the first two bytes: a
/// free record starts with [`XFS_DIR2_DATA_FREE_TAG`], which cannot be
/// the top of an inode number because inode numbers are capped at 56
/// bits. Both kinds repeat their own offset in a trailing tag, and that
/// redundancy is checked here — it is the cheapest detector for a walk
/// that has lost alignment.
fn parse_entries(buf: &[u8], sb: &Superblock, start: usize, end: usize) -> Result<Vec<DirEntry>> {
    let ftype = dir_has_ftype(sb);
    let mut entries = Vec::new();
    let mut cur = start;

    while cur < end {
        if end - cur < XFS_DIR2_DATA_ALIGN {
            return Err(Error::BadSuperblock(format!(
                "directory data block: {} bytes left at offset {cur}, too few for any record",
                end - cur
            )));
        }

        if be16(buf, cur + offsets::data_unused::FREETAG) == XFS_DIR2_DATA_FREE_TAG {
            let len = usize::from(be16(buf, cur + offsets::data_unused::LENGTH));
            if len == 0 || len % XFS_DIR2_DATA_ALIGN != 0 {
                return Err(Error::BadSuperblock(format!(
                    "directory data block: free record at {cur} has length {len}, \
                     which is zero or not 8-byte aligned"
                )));
            }
            if cur + len > end {
                return Err(Error::BadSuperblock(format!(
                    "directory data block: free record at {cur} of length {len} runs past {end}"
                )));
            }
            let tag = usize::from(be16(buf, cur + len - 2));
            if tag != cur {
                return Err(Error::BadSuperblock(format!(
                    "directory data block: free record at {cur} tags itself as {tag}"
                )));
            }
            cur += len;
            continue;
        }

        if end - cur < DATA_ENTRY_MIN_SIZE {
            return Err(Error::BadSuperblock(format!(
                "directory data block: {} bytes left at offset {cur}, too few for an entry",
                end - cur
            )));
        }
        let ino = be64(buf, cur + offsets::data_entry::INUMBER);
        let namelen = buf[cur + offsets::data_entry::NAMELEN];
        if namelen == 0 {
            return Err(Error::BadSuperblock(format!(
                "directory data block: entry at {cur} has an empty name"
            )));
        }
        let size = data_entry_size(namelen, ftype);
        if cur + size > end {
            return Err(Error::BadSuperblock(format!(
                "directory data block: entry at {cur} of length {size} runs past {end}"
            )));
        }
        let tag = usize::from(be16(buf, cur + size - 2));
        if tag != cur {
            return Err(Error::BadSuperblock(format!(
                "directory data block: entry at {cur} tags itself as {tag}"
            )));
        }
        check_entry_ino(sb, ino, &format!("directory data block entry at {cur}"))?;

        let name_start = cur + offsets::data_entry::NAME;
        let name_end = name_start + usize::from(namelen);
        let ft = if ftype {
            ftype_from_raw(buf[name_end])?
        } else {
            None
        };
        entries.push(DirEntry {
            name: buf[name_start..name_end].to_vec(),
            ino,
            ftype: ft,
            offset: cur as u32,
        });
        cur += size;
    }
    Ok(entries)
}

/// Read the hash index records at `buf[start..]`, checking the two
/// invariants XFS maintains over them: they are sorted by hash, and the
/// stale count in the header matches the number of emptied records.
fn parse_leaf_entries(
    buf: &[u8],
    start: usize,
    count: usize,
    stale: usize,
    what: &str,
) -> Result<Vec<LeafEntry>> {
    let mut entries = Vec::with_capacity(count);
    let mut previous = 0u32;
    let mut found_stale = 0usize;

    for i in 0..count {
        let off = start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        let hashval = be32(buf, off + offsets::leaf_entry::HASHVAL);
        let address = be32(buf, off + offsets::leaf_entry::ADDRESS);
        if hashval < previous {
            return Err(Error::BadSuperblock(format!(
                "{what}: hash index record {i} has hash {hashval:#010x}, \
                 below its predecessor's {previous:#010x}"
            )));
        }
        previous = hashval;
        if address == NULL_DATAPTR {
            found_stale += 1;
        }
        entries.push(LeafEntry { hashval, address });
    }

    if found_stale != stale {
        return Err(Error::BadSuperblock(format!(
            "{what}: header claims {stale} stale index records, but {found_stale} are empty"
        )));
    }
    Ok(entries)
}

/// The header size for a directory data block, chosen by its magic.
///
/// The magic also says which on-disk version wrote the block, so it is
/// checked against the superblock: a v5 block on a v4 filesystem, or the
/// reverse, means the block did not come from this filesystem.
fn data_hdr_size(magic: u32, sb: &Superblock) -> Result<usize> {
    let (size, is_v5) = match magic {
        XFS_DIR3_DATA_MAGIC | XFS_DIR3_BLOCK_MAGIC => (XFS_DIR3_DATA_HDR_SIZE, true),
        XFS_DIR2_DATA_MAGIC | XFS_DIR2_BLOCK_MAGIC => (XFS_DIR2_DATA_HDR_SIZE, false),
        other => {
            return Err(Error::BadSuperblock(format!(
                "directory data block has magic {other:#010x}, expected one of \
                 {XFS_DIR3_DATA_MAGIC:#010x}, {XFS_DIR3_BLOCK_MAGIC:#010x}, \
                 {XFS_DIR2_DATA_MAGIC:#010x} or {XFS_DIR2_BLOCK_MAGIC:#010x}"
            )))
        }
    };
    if is_v5 != sb.is_v5() {
        return Err(Error::BadSuperblock(format!(
            "directory data block magic {magic:#010x} is v{} format on a v{} filesystem",
            if is_v5 { 5 } else { 4 },
            sb.version()
        )));
    }
    Ok(size)
}

/// Whether a data block magic is the single-block (block form) variant.
fn is_block_form(magic: u32) -> bool {
    magic == XFS_DIR3_BLOCK_MAGIC || magic == XFS_DIR2_BLOCK_MAGIC
}

/// Read every entry out of one directory data block.
///
/// Accepts both the multi-block data magic (`XDD3` / `XD2D`) and the
/// single-block magic (`XDB3` / `XD2B`); for the latter the entry region
/// stops where the block's hash index begins, and
/// [`parse_block_form`] returns that index as well.
///
/// The block must be a whole `sb.dirblocksize()`, which may be larger
/// than a filesystem block.
///
/// This does not verify the v5 header — call [`verify_data_block`] for
/// that, which needs the block's address and owner from the caller.
///
/// # Errors
///
/// [`Error::BadSuperblock`] for an unrecognised magic, a magic from the
/// wrong on-disk version, a record whose length runs past the end of the
/// block, a record whose trailing tag disagrees with its own offset, an
/// empty name, an impossible inode number, or an undefined file type.
pub fn parse_data_block(buf: &[u8], sb: &Superblock) -> Result<Vec<DirEntry>> {
    let magic = read_data_magic(buf, sb)?;
    if is_block_form(magic) {
        return Ok(parse_block_form(buf, sb)?.entries);
    }
    let blocksize = dir_block_size(buf, sb)?;
    parse_entries(buf, sb, data_hdr_size(magic, sb)?, blocksize)
}

/// Read and range-check the magic at the head of a data block.
fn read_data_magic(buf: &[u8], sb: &Superblock) -> Result<u32> {
    let blocksize = dir_block_size(buf, sb)?;
    let smallest = XFS_DIR2_DATA_HDR_SIZE + XFS_DIR2_BLOCK_TAIL_SIZE;
    if blocksize < smallest {
        return Err(Error::BadSuperblock(format!(
            "directory block size {blocksize} is smaller than the smallest possible header"
        )));
    }
    Ok(be32(buf, offsets::dir3_blk::MAGIC))
}

/// Read a block-form directory: one directory block holding the entries
/// and the whole hash index.
///
/// The block ends with an `xfs_dir2_block_tail` — the index record count
/// and stale count — preceded by the index records themselves. Entries
/// occupy everything between the header and the first index record.
///
/// # Errors
///
/// As [`parse_data_block`], plus [`Error::BadSuperblock`] when the index
/// record count does not fit between the header and the tail, or when
/// the index is unsorted or its stale count is wrong.
pub fn parse_block_form(buf: &[u8], sb: &Superblock) -> Result<BlockDir> {
    let magic = read_data_magic(buf, sb)?;
    if !is_block_form(magic) {
        return Err(Error::BadSuperblock(format!(
            "block-form directory has magic {magic:#010x}, expected \
             {XFS_DIR3_BLOCK_MAGIC:#010x} or {XFS_DIR2_BLOCK_MAGIC:#010x}"
        )));
    }
    let blocksize = dir_block_size(buf, sb)?;
    let hdr_size = data_hdr_size(magic, sb)?;
    if blocksize < hdr_size + XFS_DIR2_BLOCK_TAIL_SIZE {
        return Err(Error::BadSuperblock(format!(
            "directory block size {blocksize} leaves no room for a {hdr_size}-byte header \
             and its tail"
        )));
    }

    // The tail sits at the very end of the block; the index records run
    // backwards from it.
    let tail = blocksize - XFS_DIR2_BLOCK_TAIL_SIZE;
    let count = be32(buf, tail + offsets::block_tail::COUNT) as usize;
    let stale = be32(buf, tail + offsets::block_tail::STALE) as usize;
    let index_bytes = count.saturating_mul(XFS_DIR2_LEAF_ENTRY_SIZE);
    if index_bytes > tail - hdr_size {
        return Err(Error::BadSuperblock(format!(
            "block-form directory claims {count} index records ({index_bytes} bytes), \
             which do not fit between the header and the tail"
        )));
    }
    if stale > count {
        return Err(Error::BadSuperblock(format!(
            "block-form directory claims {stale} stale records out of {count}"
        )));
    }
    let index_start = tail - index_bytes;

    let index = parse_leaf_entries(buf, index_start, count, stale, "block-form directory")?;
    let entries = parse_entries(buf, sb, hdr_size, index_start)?;
    Ok(BlockDir { entries, index })
}

/// The header size for a leaf or node block, chosen by its magic, with
/// the same cross-version check as [`data_hdr_size`].
fn da_hdr_size(magic: u16, sb: &Superblock, what: &str) -> Result<usize> {
    let (size, is_v5) = match magic {
        XFS_DIR3_LEAF1_MAGIC | XFS_DIR3_LEAFN_MAGIC => (XFS_DIR3_LEAF_HDR_SIZE, true),
        XFS_DIR2_LEAF1_MAGIC | XFS_DIR2_LEAFN_MAGIC => (XFS_DIR2_LEAF_HDR_SIZE, false),
        XFS_DA3_NODE_MAGIC => (XFS_DA3_NODE_HDR_SIZE, true),
        XFS_DA_NODE_MAGIC => (XFS_DA_NODE_HDR_SIZE, false),
        other => {
            return Err(Error::BadSuperblock(format!(
                "{what} has magic {other:#06x}, which is not a directory index magic"
            )))
        }
    };
    if is_v5 != sb.is_v5() {
        return Err(Error::BadSuperblock(format!(
            "{what} magic {magic:#06x} is v{} format on a v{} filesystem",
            if is_v5 { 5 } else { 4 },
            sb.version()
        )));
    }
    Ok(size)
}

/// Read one leaf block — the hash index of a leaf-form directory, or one
/// leaf of a node-form directory's B-tree.
///
/// Both are the same structure; the magic distinguishes them, and only
/// the leaf-form one carries a "best free" array in its tail.
///
/// This does not verify the v5 header — call [`verify_da_block`].
///
/// # Errors
///
/// [`Error::BadSuperblock`] for a magic that is not a leaf magic or
/// comes from the wrong on-disk version, a record count that does not
/// fit in the block, an unsorted index, or a stale count that disagrees
/// with the records.
pub fn parse_leaf(buf: &[u8], sb: &Superblock) -> Result<DirLeaf> {
    let blocksize = dir_block_size(buf, sb)?;
    if blocksize < XFS_DIR3_LEAF_HDR_SIZE {
        return Err(Error::BadSuperblock(format!(
            "directory block size {blocksize} is too small to hold a leaf header"
        )));
    }
    // In both versions the magic is the third field of `xfs_da_blkinfo`,
    // after the forward and backward block pointers.
    let magic = be16(buf, offsets::da_blk::MAGIC);
    if !matches!(
        magic,
        XFS_DIR3_LEAF1_MAGIC | XFS_DIR3_LEAFN_MAGIC | XFS_DIR2_LEAF1_MAGIC | XFS_DIR2_LEAFN_MAGIC
    ) {
        return Err(Error::BadSuperblock(format!(
            "directory leaf block has magic {magic:#06x}, expected one of \
             {XFS_DIR3_LEAF1_MAGIC:#06x}, {XFS_DIR3_LEAFN_MAGIC:#06x}, \
             {XFS_DIR2_LEAF1_MAGIC:#06x} or {XFS_DIR2_LEAFN_MAGIC:#06x}"
        )));
    }
    let hdr_size = da_hdr_size(magic, sb, "directory leaf block")?;
    // count and stale sit immediately after the block info header.
    let counts = offsets::da_counts(hdr_size, sb.is_v5());
    let count = be16(buf, counts);
    let stale = be16(buf, counts + 2);
    if stale > count {
        return Err(Error::BadSuperblock(format!(
            "directory leaf block claims {stale} stale records out of {count}"
        )));
    }

    // A leaf-form directory's single index block ends with a "best free"
    // array and its length; the leaves of a node-form directory do not.
    let single = magic == XFS_DIR3_LEAF1_MAGIC || magic == XFS_DIR2_LEAF1_MAGIC;
    let (bestcount, limit) = if single {
        let tail = blocksize - XFS_DIR2_LEAF_TAIL_SIZE;
        let bestcount = be32(buf, tail);
        let bests_bytes = (bestcount as usize).saturating_mul(2);
        if bests_bytes > tail - hdr_size {
            return Err(Error::BadSuperblock(format!(
                "leaf-form directory claims {bestcount} best-free records, \
                 which do not fit in the block"
            )));
        }
        (Some(bestcount), tail - bests_bytes)
    } else {
        (None, blocksize)
    };

    let index_bytes = usize::from(count).saturating_mul(XFS_DIR2_LEAF_ENTRY_SIZE);
    if hdr_size + index_bytes > limit {
        return Err(Error::BadSuperblock(format!(
            "directory leaf block claims {count} index records, which do not fit \
             between its header and offset {limit}"
        )));
    }
    let entries = parse_leaf_entries(
        buf,
        hdr_size,
        usize::from(count),
        usize::from(stale),
        "directory leaf block",
    )?;

    Ok(DirLeaf {
        magic,
        forw: be32(buf, offsets::da_blk::FORW),
        back: be32(buf, offsets::da_blk::BACK),
        count,
        stale,
        entries,
        bestcount,
    })
}

/// Read one interior node of a node-form directory's hash B-tree.
///
/// Only the node itself is read. Descending is the caller's loop:
/// [`DirNode::child_for_hash`] names the next directory block, and this
/// module neither reads it nor knows how to map it to a disk address.
///
/// # Errors
///
/// [`Error::BadSuperblock`] for a magic that is not a node magic or
/// comes from the wrong on-disk version, a level of 0 (which would make
/// it a leaf, not a node) or above `XFS_DA_NODE_MAXDEPTH`, a record
/// count that does not fit in the block, or an unsorted index.
///
/// [`Error::UnsupportedFeature`] when the node sits more than
/// [`MAX_SUPPORTED_NODE_LEVEL`] levels above the leaves. Such a tree is
/// well-formed and this driver simply will not walk it: no fixture
/// available here produces one, so descending it would be untested
/// guesswork rather than a read path. It is refused loudly instead of
/// returning a partial answer.
pub fn parse_node(buf: &[u8], sb: &Superblock) -> Result<DirNode> {
    let blocksize = dir_block_size(buf, sb)?;
    if blocksize < XFS_DA3_NODE_HDR_SIZE {
        return Err(Error::BadSuperblock(format!(
            "directory block size {blocksize} is too small to hold a node header"
        )));
    }
    let magic = be16(buf, offsets::da_blk::MAGIC);
    if magic != XFS_DA3_NODE_MAGIC && magic != XFS_DA_NODE_MAGIC {
        return Err(Error::BadSuperblock(format!(
            "directory node block has magic {magic:#06x}, expected \
             {XFS_DA3_NODE_MAGIC:#06x} or {XFS_DA_NODE_MAGIC:#06x}"
        )));
    }
    let hdr_size = da_hdr_size(magic, sb, "directory node block")?;
    // count and level sit immediately after the block info header, in
    // the same slots the leaf header uses for count and stale.
    let counts = offsets::da_counts(hdr_size, sb.is_v5());
    let count = be16(buf, counts);
    let level = be16(buf, counts + 2);

    if level == 0 {
        return Err(Error::BadSuperblock(
            "directory node block is at level 0, which is a leaf, not a node".into(),
        ));
    }
    if level > XFS_DA_NODE_MAXDEPTH {
        return Err(Error::BadSuperblock(format!(
            "directory node block is at level {level}, deeper than the \
             {XFS_DA_NODE_MAXDEPTH}-level maximum the format allows"
        )));
    }
    if level > MAX_SUPPORTED_NODE_LEVEL {
        return Err(Error::UnsupportedFeature(format!(
            "directory hash B-tree is {level} levels above its leaves; this driver \
             descends at most {MAX_SUPPORTED_NODE_LEVEL}"
        )));
    }

    let index_bytes = usize::from(count).saturating_mul(XFS_DA_NODE_ENTRY_SIZE);
    if hdr_size + index_bytes > blocksize {
        return Err(Error::BadSuperblock(format!(
            "directory node block claims {count} child records, which do not fit \
             in a {blocksize}-byte block"
        )));
    }

    let mut entries = Vec::with_capacity(usize::from(count));
    let mut previous = 0u32;
    for i in 0..usize::from(count) {
        let off = hdr_size + i * XFS_DA_NODE_ENTRY_SIZE;
        let hashval = be32(buf, off + offsets::node_entry::HASHVAL);
        if hashval < previous {
            return Err(Error::BadSuperblock(format!(
                "directory node block: child {i} has hash {hashval:#010x}, \
                 below its predecessor's {previous:#010x}"
            )));
        }
        previous = hashval;
        entries.push(NodeEntry {
            hashval,
            before: be32(buf, off + offsets::node_entry::BEFORE),
        });
    }

    Ok(DirNode {
        level,
        count,
        forw: be32(buf, offsets::da_blk::FORW),
        back: be32(buf, offsets::da_blk::BACK),
        entries,
    })
}

/// Read a short-form inode number, four or eight bytes wide.
fn read_sf_ino(buf: &[u8], off: usize, wide: bool) -> Result<u64> {
    if !wide {
        return Ok(u64::from(be32(buf, off)));
    }
    let raw = be64(buf, off);
    // XFS caps inode numbers at 56 bits, so the top byte is always zero.
    // The kernel masks it off unconditionally; this driver rejects it
    // instead, because on a valid volume the byte cannot be set and a set
    // byte is far better evidence of a misaligned read than of data worth
    // salvaging.
    if raw > MAX_INUMBER {
        return Err(Error::BadSuperblock(format!(
            "short-form directory: inode number {raw:#018x} sets bits above the \
             56-bit maximum"
        )));
    }
    Ok(raw)
}

/// Read a short-form directory out of an inode's data fork.
///
/// `fork` is the inode's data fork, as delimited by
/// [`Inode::data_fork_range`]; `inode.size` gives the exact number of
/// bytes the directory occupies within it.
///
/// # Layout
///
/// A header of a 1-byte entry count, a 1-byte count of wide inode
/// numbers, and the parent inode; then the entries, byte-packed with no
/// alignment. Each entry is a 1-byte name length, a 2-byte offset, the
/// name, an optional file type byte, and the inode number.
///
/// **The inode number width is a property of the directory, not of the
/// entry.** When the header's `i8count` is non-zero every inode number
/// in the directory — the parent's included — is stored in eight bytes;
/// when it is zero every one is stored in four. `i8count` counts how
/// many of them actually need the width, and XFS rewrites the whole
/// directory when that count crosses zero. Treating it as a per-entry
/// flag misreads every entry after the first wide one.
///
/// # Errors
///
/// [`Error::NotADirectory`] if the inode is not a directory, and
/// [`Error::BadSuperblock`] if the fork is not in local format, if the
/// entries do not exactly fill `inode.size`, if an entry has an empty
/// name or an impossible inode number, if the file type byte is
/// undefined, or if the recomputed count of wide inode numbers
/// disagrees with the header's. That last check is the redundancy XFS
/// carries for exactly this purpose: it fails loudly if the inode
/// numbers were read at the wrong width or the wrong offset.
pub fn read_short_form(inode: &Inode, fork: &[u8], sb: &Superblock) -> Result<ShortFormDir> {
    if !inode.is_dir() {
        return Err(Error::NotADirectory);
    }
    if inode.format != Format::Local {
        return Err(Error::BadSuperblock(format!(
            "inode {}: directory fork format is {:?}, not the short (local) form",
            inode.ino, inode.format
        )));
    }
    let size = usize::try_from(inode.size).map_err(|_| {
        Error::BadSuperblock(format!(
            "inode {}: short-form directory size {} is not addressable",
            inode.ino, inode.size
        ))
    })?;
    if size > fork.len() {
        return Err(Error::BadSuperblock(format!(
            "inode {}: short-form directory claims {size} bytes but its data fork holds {}",
            inode.ino,
            fork.len()
        )));
    }
    let sf = &fork[..size];

    // The header is the two counts plus a parent inode number whose
    // width i8count selects: 6 bytes narrow, 10 bytes wide.
    if sf.len() < 2 {
        return Err(Error::BadSuperblock(format!(
            "inode {}: short-form directory is {} bytes, too short for its header",
            inode.ino,
            sf.len()
        )));
    }
    let count = usize::from(sf[offsets::sf_hdr::COUNT]);
    let i8count = sf[offsets::sf_hdr::I8COUNT];
    let wide = i8count != 0;
    let ino_size = if wide { 8 } else { 4 };
    let hdr_size = offsets::sf_hdr::PARENT + ino_size;
    if sf.len() < hdr_size {
        return Err(Error::BadSuperblock(format!(
            "inode {}: short-form directory is {} bytes, too short for a {hdr_size}-byte header",
            inode.ino,
            sf.len()
        )));
    }

    let parent_ino = read_sf_ino(sf, offsets::sf_hdr::PARENT, wide)?;
    check_entry_ino(
        sb,
        parent_ino,
        &format!("inode {}: short-form parent", inode.ino),
    )?;

    let ftype = dir_has_ftype(sb);
    let mut entries = Vec::with_capacity(count);
    // The header's i8count covers the parent as well as the entries.
    let mut found_wide = usize::from(parent_ino > MAX_SHORT_INUM);
    let mut cur = hdr_size;

    for i in 0..count {
        if sf.len() - cur < offsets::sf_entry::NAME {
            return Err(Error::BadSuperblock(format!(
                "inode {}: short-form entry {i} starts at {cur}, past the end of the directory",
                inode.ino
            )));
        }
        let namelen = usize::from(sf[cur + offsets::sf_entry::NAMELEN]);
        if namelen == 0 {
            return Err(Error::BadSuperblock(format!(
                "inode {}: short-form entry {i} has an empty name",
                inode.ino
            )));
        }
        let offset = be16(sf, cur + offsets::sf_entry::OFFSET);
        let entsize = offsets::sf_entry::NAME + namelen + usize::from(ftype) + ino_size;
        if cur + entsize > sf.len() {
            return Err(Error::BadSuperblock(format!(
                "inode {}: short-form entry {i} of length {entsize} runs past the \
                 {}-byte directory",
                inode.ino,
                sf.len()
            )));
        }

        let name_start = cur + offsets::sf_entry::NAME;
        let name_end = name_start + namelen;
        let ft = if ftype {
            ftype_from_raw(sf[name_end])?
        } else {
            None
        };
        let ino = read_sf_ino(sf, name_end + usize::from(ftype), wide)?;
        check_entry_ino(
            sb,
            ino,
            &format!("inode {}: short-form entry {i}", inode.ino),
        )?;
        if ino > MAX_SHORT_INUM {
            found_wide += 1;
        }

        entries.push(DirEntry {
            name: sf[name_start..name_end].to_vec(),
            ino,
            ftype: ft,
            offset: u32::from(offset),
        });
        cur += entsize;
    }

    // The entries must end exactly where the fork does. A short-form
    // directory carries no padding, so anything left over means an entry
    // was measured wrongly -- most likely the file type byte or the
    // inode number width.
    if cur != sf.len() {
        return Err(Error::BadSuperblock(format!(
            "inode {}: short-form directory's {count} entries end at {cur}, not at its \
             declared size of {}",
            inode.ino,
            sf.len()
        )));
    }
    if found_wide != usize::from(i8count) {
        return Err(Error::BadSuperblock(format!(
            "inode {}: short-form header claims {i8count} inode numbers wider than \
             32 bits, but {found_wide} are",
            inode.ino
        )));
    }

    Ok(ShortFormDir {
        parent_ino,
        i8count,
        entries,
    })
}

#[cfg(test)]
mod tests {
    //! These fixtures are built in-process, so they prove the parser is
    //! self-consistent and nothing more. A misreading of the format is
    //! baked into the builder and the parser alike, and the two agree
    //! with each other while disagreeing with every real filesystem.
    //! Correctness is established by `tests/dir_oracle.rs`, which runs
    //! this parser over filesystems `mkfs.xfs` produced. See AGENTS.md.
    //!
    //! One thing the builders here deliberately do not do is compute
    //! name hashes. XFS's directory hash lives in the kernel, and this
    //! crate takes no code from it; the index fixtures use synthetic,
    //! ascending hash values, which exercises the ordering and staleness
    //! checks but says nothing about hash agreement with XFS.

    use super::*;
    use crate::inode::XFS_DINODE_MAGIC;
    use crate::superblock::XFS_SB_MAGIC;

    /// Allocation group geometry: `(agcount, agblocks, agblklog)`.
    ///
    /// The two shapes matter because an inode number packs its AG index
    /// into its high bits, so which inode numbers are *legal* is a
    /// function of the geometry. Directory entries pointing outside the
    /// filesystem are rejected, and a test that wants a 64-bit inode
    /// number needs a filesystem large enough to have one.
    type Ags = (u32, u32, u8);

    /// A small filesystem: 4 AGs of 1000 blocks.
    const SMALL_AGS: Ags = (4, 1000, 10);

    /// A large one: 64 AGs of 32M 4 KiB blocks, roughly 8 TiB. Inode
    /// numbers here run past 32 bits, which is the only way to reach the
    /// short-form 8-byte representation honestly.
    const LARGE_AGS: Ags = (64, 32_000_000, 25);

    /// Build a superblock with the given on-disk version, feature masks
    /// and AG geometry: 4 KiB blocks and directory blocks, 512-byte
    /// sectors and inodes.
    fn build_sb(version: u16, incompat: u32, features2: u32, ags: Ags) -> Superblock {
        let (agcount, agblocks, agblklog) = ags;
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
        let dblocks = u64::from(agcount) * u64::from(agblocks);
        b[8..16].copy_from_slice(&dblocks.to_be_bytes()); // dblocks
        b[48..56].copy_from_slice(&100u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&agblocks.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&agcount.to_be_bytes()); // agcount
        let versionnum = version | version_flags::MOREBITSBIT;
        b[100..102].copy_from_slice(&versionnum.to_be_bytes());
        b[102..104].copy_from_slice(&512u16.to_be_bytes()); // sectsize
        b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        b[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
        b[120] = 12; // blocklog
        b[121] = 9; // sectlog
        b[122] = 9; // inodelog
        b[123] = 3; // inopblog
        b[124] = agblklog; // agblklog
        b[192] = 0; // dirblklog: directory blocks are one fs block
        b[200..204].copy_from_slice(&features2.to_be_bytes());
        b[216..220].copy_from_slice(&incompat.to_be_bytes());
        // Distinctive UUID so identity mismatches are unambiguous.
        for (i, slot) in b[32..48].iter_mut().enumerate() {
            *slot = i as u8;
        }
        let crc = crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).unwrap()
    }

    /// v5 with the file-type feature, which is what mkfs produces.
    fn sb_v5_ftype() -> Superblock {
        build_sb(5, crate::superblock::incompat::FTYPE, 0, SMALL_AGS)
    }

    /// v5 with the file-type feature, on a filesystem big enough to have
    /// inode numbers that do not fit in 32 bits.
    fn sb_v5_large() -> Superblock {
        build_sb(5, crate::superblock::incompat::FTYPE, 0, LARGE_AGS)
    }

    /// v5 without the file-type feature.
    fn sb_v5_noftype() -> Superblock {
        build_sb(5, 0, 0, SMALL_AGS)
    }

    /// v4 with the file-type feature, which v4 advertises in
    /// `sb_features2` rather than in the incompatible mask.
    fn sb_v4_ftype() -> Superblock {
        build_sb(4, 0, SB_VERSION2_FTYPE, SMALL_AGS)
    }

    /// v4 without the file-type feature.
    fn sb_v4_noftype() -> Superblock {
        build_sb(4, 0, 0, SMALL_AGS)
    }

    /// A v3 directory inode whose data fork holds `fork`.
    fn dir_inode(sb: &Superblock, ino: u64, fork: &[u8]) -> Inode {
        let mut b = vec![0u8; usize::from(sb.inodesize)];
        b[0..2].copy_from_slice(&XFS_DINODE_MAGIC.to_be_bytes());
        b[2..4].copy_from_slice(&0o040755u16.to_be_bytes()); // directory
        b[4] = if sb.is_v5() { 3 } else { 2 };
        b[5] = 1; // format: local
        b[16..20].copy_from_slice(&2u32.to_be_bytes()); // nlink
        b[56..64].copy_from_slice(&(fork.len() as u64).to_be_bytes()); // size
        b[83] = 2; // aformat: extents
        b[96..100].copy_from_slice(&u32::MAX.to_be_bytes()); // next_unlinked
        let core = if sb.is_v5() { 176 } else { 100 };
        b[core..core + fork.len()].copy_from_slice(fork);
        if sb.is_v5() {
            b[152..160].copy_from_slice(&ino.to_be_bytes());
            b[160..176].copy_from_slice(&sb.meta_uuid);
            let crc = crc32c_with_zeroed_crc(&b, 100);
            b[100..104].copy_from_slice(&crc.to_le_bytes());
        }
        Inode::parse(&b, sb, ino).unwrap()
    }

    /// Read a short-form directory that was built for `sb`, wrapping it
    /// in an inode the way a real read would.
    fn read_sf(sb: &Superblock, fork: &[u8]) -> Result<ShortFormDir> {
        let inode = dir_inode(sb, 128, fork);
        let (start, end) = inode.data_fork_range(usize::from(sb.inodesize));
        let mut buf = vec![0u8; usize::from(sb.inodesize)];
        buf[start..start + fork.len()].copy_from_slice(fork);
        read_short_form(&inode, &buf[start..end], sb)
    }

    /// Build a short-form directory fork.
    ///
    /// `wide` selects the 8-byte inode representation for the whole
    /// directory, as `i8count` does on disk; `i8count` is written from
    /// the number of inode numbers that actually exceed 32 bits.
    fn build_sf(sb: &Superblock, parent: u64, entries: &[(&str, u64, u8)], wide: bool) -> Vec<u8> {
        let ftype = dir_has_ftype(sb);
        let mut i8count = u8::from(parent > MAX_SHORT_INUM);
        let mut b = vec![entries.len() as u8, 0];
        push_sf_ino(&mut b, parent, wide);
        for (name, ino, ft) in entries {
            b.push(name.len() as u8);
            b.extend_from_slice(&0u16.to_be_bytes()); // offset cookie
            b.extend_from_slice(name.as_bytes());
            if ftype {
                b.push(*ft);
            }
            push_sf_ino(&mut b, *ino, wide);
            if *ino > MAX_SHORT_INUM {
                i8count += 1;
            }
        }
        b[1] = i8count;
        b
    }

    fn push_sf_ino(b: &mut Vec<u8>, ino: u64, wide: bool) {
        if wide {
            b.extend_from_slice(&ino.to_be_bytes());
        } else {
            b.extend_from_slice(&(ino as u32).to_be_bytes());
        }
    }

    /// Append one directory data-block entry to `b`, padded and tagged
    /// the way XFS stores it.
    fn push_data_entry(b: &mut Vec<u8>, ftype: bool, offset: usize, name: &[u8], ino: u64, ft: u8) {
        let size = data_entry_size(name.len() as u8, ftype);
        let start = b.len();
        b.extend_from_slice(&ino.to_be_bytes());
        b.push(name.len() as u8);
        b.extend_from_slice(name);
        if ftype {
            b.push(ft);
        }
        b.resize(start + size, 0);
        // The trailing tag repeats the entry's own offset in the block.
        b[start + size - 2..start + size].copy_from_slice(&(offset as u16).to_be_bytes());
    }

    /// Write a free record covering `buf[from..to]`.
    fn put_unused(buf: &mut [u8], from: usize, to: usize) {
        let len = to - from;
        buf[from..from + 2].copy_from_slice(&XFS_DIR2_DATA_FREE_TAG.to_be_bytes());
        buf[from + 2..from + 4].copy_from_slice(&(len as u16).to_be_bytes());
        buf[to - 2..to].copy_from_slice(&(from as u16).to_be_bytes());
    }

    /// Stamp a v5 `xfs_dir3_blk_hdr` and its CRC over a finished block.
    fn seal_dir3(buf: &mut [u8], sb: &Superblock, daddr: u64, owner: u64) {
        if !sb.is_v5() {
            return;
        }
        buf[DIR3_BLK_HDR.blkno..DIR3_BLK_HDR.blkno + 8].copy_from_slice(&daddr.to_be_bytes());
        buf[DIR3_BLK_HDR.uuid..DIR3_BLK_HDR.uuid + 16].copy_from_slice(&sb.meta_uuid);
        buf[DIR3_BLK_HDR.owner..DIR3_BLK_HDR.owner + 8].copy_from_slice(&owner.to_be_bytes());
        let crc = crc32c_with_zeroed_crc(buf, DIR3_BLK_HDR.crc);
        buf[DIR3_BLK_HDR.crc..DIR3_BLK_HDR.crc + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Stamp a v5 `xfs_da3_blkinfo` and its CRC over a finished block.
    fn seal_da3(buf: &mut [u8], sb: &Superblock, daddr: u64, owner: u64) {
        if !sb.is_v5() {
            return;
        }
        buf[DA3_BLK_HDR.blkno..DA3_BLK_HDR.blkno + 8].copy_from_slice(&daddr.to_be_bytes());
        buf[DA3_BLK_HDR.uuid..DA3_BLK_HDR.uuid + 16].copy_from_slice(&sb.meta_uuid);
        buf[DA3_BLK_HDR.owner..DA3_BLK_HDR.owner + 8].copy_from_slice(&owner.to_be_bytes());
        let crc = crc32c_with_zeroed_crc(buf, DA3_BLK_HDR.crc);
        buf[DA3_BLK_HDR.crc..DA3_BLK_HDR.crc + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// Build a whole block-form directory: header, `.`, `..`, the given
    /// names, a free record filling the gap, then the hash index and the
    /// tail.
    fn build_block_dir(sb: &Superblock, owner: u64, names: &[(&str, u64, u8)]) -> Vec<u8> {
        let blocksize = sb.dirblocksize() as usize;
        let ftype = dir_has_ftype(sb);
        let hdr_size = if sb.is_v5() {
            XFS_DIR3_DATA_HDR_SIZE
        } else {
            XFS_DIR2_DATA_HDR_SIZE
        };
        let magic = if sb.is_v5() {
            XFS_DIR3_BLOCK_MAGIC
        } else {
            XFS_DIR2_BLOCK_MAGIC
        };

        // Unlike short form, every other shape stores `.` and `..` as
        // ordinary entries.
        let mut all: Vec<(&str, u64, u8)> = vec![(".", owner, 2), ("..", 128, 2)];
        all.extend_from_slice(names);

        let mut body: Vec<u8> = vec![0u8; hdr_size];
        let mut offsets = Vec::new();
        for (name, ino, ft) in &all {
            let at = body.len();
            offsets.push(at);
            push_data_entry(&mut body, ftype, at, name.as_bytes(), *ino, *ft);
        }

        let count = offsets.len();
        let mut buf = vec![0u8; blocksize];
        buf[..body.len()].copy_from_slice(&body);
        buf[0..4].copy_from_slice(&magic.to_be_bytes());

        let tail = blocksize - XFS_DIR2_BLOCK_TAIL_SIZE;
        let index_start = tail - count * XFS_DIR2_LEAF_ENTRY_SIZE;
        put_unused(&mut buf, body.len(), index_start);
        for (i, off) in offsets.iter().enumerate() {
            let at = index_start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
            buf[at..at + 4].copy_from_slice(&(i as u32).to_be_bytes());
            // Addresses are in 8-byte units from the start of the
            // directory's data space.
            buf[at + 4..at + 8].copy_from_slice(&((*off as u32) >> 3).to_be_bytes());
        }
        buf[tail..tail + 4].copy_from_slice(&(count as u32).to_be_bytes());
        buf[tail + 4..tail + 8].copy_from_slice(&0u32.to_be_bytes()); // stale
        seal_dir3(&mut buf, sb, 42, owner);
        buf
    }

    /// Build a standalone directory data block: header, the given names,
    /// then a free record covering the rest.
    fn build_data_block(sb: &Superblock, owner: u64, names: &[(&str, u64, u8)]) -> Vec<u8> {
        let blocksize = sb.dirblocksize() as usize;
        let ftype = dir_has_ftype(sb);
        let hdr_size = if sb.is_v5() {
            XFS_DIR3_DATA_HDR_SIZE
        } else {
            XFS_DIR2_DATA_HDR_SIZE
        };
        let magic = if sb.is_v5() {
            XFS_DIR3_DATA_MAGIC
        } else {
            XFS_DIR2_DATA_MAGIC
        };

        let mut body: Vec<u8> = vec![0u8; hdr_size];
        for (name, ino, ft) in names {
            let at = body.len();
            push_data_entry(&mut body, ftype, at, name.as_bytes(), *ino, *ft);
        }
        let mut buf = vec![0u8; blocksize];
        buf[..body.len()].copy_from_slice(&body);
        buf[0..4].copy_from_slice(&magic.to_be_bytes());
        put_unused(&mut buf, body.len(), blocksize);
        seal_dir3(&mut buf, sb, 42, owner);
        buf
    }

    /// Build a leaf block with `count` ascending index records.
    fn build_leaf(sb: &Superblock, owner: u64, magic: u16, count: u16, stale: u16) -> Vec<u8> {
        let blocksize = sb.dirblocksize() as usize;
        let hdr_size = if sb.is_v5() {
            XFS_DIR3_LEAF_HDR_SIZE
        } else {
            XFS_DIR2_LEAF_HDR_SIZE
        };
        let mut buf = vec![0u8; blocksize];
        let m = offsets::da_blk::MAGIC;
        buf[m..m + 2].copy_from_slice(&magic.to_be_bytes());
        let counts = offsets::da_counts(hdr_size, sb.is_v5());
        buf[counts..counts + 2].copy_from_slice(&count.to_be_bytes());
        buf[counts + 2..counts + 4].copy_from_slice(&stale.to_be_bytes());
        for i in 0..usize::from(count) {
            let at = hdr_size + i * XFS_DIR2_LEAF_ENTRY_SIZE;
            buf[at..at + 4].copy_from_slice(&(i as u32).to_be_bytes());
            let address = if i < usize::from(stale) {
                NULL_DATAPTR
            } else {
                (i as u32) + 1
            };
            buf[at + 4..at + 8].copy_from_slice(&address.to_be_bytes());
        }
        if magic == XFS_DIR3_LEAF1_MAGIC || magic == XFS_DIR2_LEAF1_MAGIC {
            let tail = blocksize - XFS_DIR2_LEAF_TAIL_SIZE;
            buf[tail..tail + 4].copy_from_slice(&3u32.to_be_bytes()); // bestcount
        }
        seal_da3(&mut buf, sb, 42, owner);
        buf
    }

    /// Build a B-tree node block with `count` ascending child records.
    fn build_node(sb: &Superblock, owner: u64, level: u16, count: u16) -> Vec<u8> {
        let blocksize = sb.dirblocksize() as usize;
        let hdr_size = if sb.is_v5() {
            XFS_DA3_NODE_HDR_SIZE
        } else {
            XFS_DA_NODE_HDR_SIZE
        };
        let magic = if sb.is_v5() {
            XFS_DA3_NODE_MAGIC
        } else {
            XFS_DA_NODE_MAGIC
        };
        let mut buf = vec![0u8; blocksize];
        let m = offsets::da_blk::MAGIC;
        buf[m..m + 2].copy_from_slice(&magic.to_be_bytes());
        let counts = offsets::da_counts(hdr_size, sb.is_v5());
        buf[counts..counts + 2].copy_from_slice(&count.to_be_bytes());
        buf[counts + 2..counts + 4].copy_from_slice(&level.to_be_bytes());
        for i in 0..usize::from(count) {
            let at = hdr_size + i * XFS_DA_NODE_ENTRY_SIZE;
            buf[at..at + 4].copy_from_slice(&(((i as u32) + 1) * 100).to_be_bytes());
            buf[at + 4..at + 8].copy_from_slice(&((i as u32) + 10).to_be_bytes());
        }
        seal_da3(&mut buf, sb, 42, owner);
        buf
    }

    fn names(entries: &[DirEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect()
    }

    // -- magics ------------------------------------------------------

    /// The magics are ASCII, and the constants must spell it. Two
    /// transposed bytes in the superblock magic shipped once already.
    #[test]
    fn magics_spell_their_ascii() {
        assert_eq!(&XFS_DIR3_BLOCK_MAGIC.to_be_bytes(), b"XDB3");
        assert_eq!(&XFS_DIR3_DATA_MAGIC.to_be_bytes(), b"XDD3");
        assert_eq!(&XFS_DIR3_FREE_MAGIC.to_be_bytes(), b"XDF3");
        assert_eq!(&XFS_DIR2_BLOCK_MAGIC.to_be_bytes(), b"XD2B");
        assert_eq!(&XFS_DIR2_DATA_MAGIC.to_be_bytes(), b"XD2D");
        assert_eq!(&XFS_DIR2_FREE_MAGIC.to_be_bytes(), b"XD2F");
    }

    // -- short form --------------------------------------------------

    #[test]
    fn parses_short_form_with_ftype() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[("hello", 1024, 1), ("subdir", 2048, 2)], false);
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.parent_ino, 128);
        assert_eq!(dir.i8count, 0);
        assert_eq!(names(&dir.entries), ["hello", "subdir"]);
        assert_eq!(dir.entries[0].ino, 1024);
        assert_eq!(dir.entries[0].ftype, Some(FileType::Regular));
        assert_eq!(dir.entries[1].ftype, Some(FileType::Directory));
    }

    /// Without the feature there is no file type byte, so every entry is
    /// one byte shorter. Getting this wrong shifts the inode number.
    #[test]
    fn parses_short_form_without_ftype() {
        let sb = sb_v5_noftype();
        let fork = build_sf(&sb, 128, &[("hello", 1024, 0), ("world", 2048, 0)], false);
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(names(&dir.entries), ["hello", "world"]);
        assert_eq!(dir.entries[0].ino, 1024);
        assert_eq!(dir.entries[1].ino, 2048);
        assert!(dir.entries.iter().all(|e| e.ftype.is_none()));
    }

    /// A v4 filesystem advertises the file type feature in
    /// `sb_features2`, not in the v5 incompatible mask. The `xfs-nocrc`
    /// oracle fixture is exactly this shape.
    #[test]
    fn v4_ftype_comes_from_features2() {
        assert!(dir_has_ftype(&sb_v4_ftype()));
        assert!(!dir_has_ftype(&sb_v4_noftype()));
        assert!(dir_has_ftype(&sb_v5_ftype()));
        assert!(!dir_has_ftype(&sb_v5_noftype()));

        let sb = sb_v4_ftype();
        let fork = build_sf(&sb, 128, &[("f", 1024, 1)], false);
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.entries[0].ino, 1024);
        assert_eq!(dir.entries[0].ftype, Some(FileType::Regular));
    }

    #[test]
    fn parses_empty_short_form() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[], false);
        // The header alone: two counts and a 4-byte parent inode. Real
        // filesystems report exactly this size for a fresh root.
        assert_eq!(fork.len(), 6);
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.parent_ino, 128);
        assert!(dir.entries.is_empty());
    }

    /// When any inode number needs more than 32 bits the whole directory
    /// switches to the 8-byte representation -- parent included.
    #[test]
    fn parses_short_form_with_wide_inode_numbers() {
        let sb = sb_v5_large();
        // AG 32 of 64, comfortably inside the filesystem and comfortably
        // past what 32 bits can hold.
        let wide_ino = 1u64 << 33;
        let fork = build_sf(
            &sb,
            128,
            &[("narrow", 1024, 1), ("wide", wide_ino, 1)],
            true,
        );
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.i8count, 1, "only the one entry needs the extra width");
        assert_eq!(dir.parent_ino, 128);
        assert_eq!(
            dir.entries[0].ino, 1024,
            "narrow inodes still widen to 8 bytes"
        );
        assert_eq!(dir.entries[1].ino, wide_ino);
    }

    /// A wide parent counts toward i8count too.
    #[test]
    fn wide_parent_counts_toward_i8count() {
        let sb = sb_v5_large();
        let wide_parent = 1u64 << 33;
        let fork = build_sf(&sb, wide_parent, &[("a", 1024, 1)], true);
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.i8count, 1);
        assert_eq!(dir.parent_ino, wide_parent);
    }

    #[test]
    fn short_form_offset_cookie_is_preserved() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("a", 1024, 1)], false);
        // The 2-byte offset follows the name length in each entry.
        fork[7..9].copy_from_slice(&0x1234u16.to_be_bytes());
        let dir = read_sf(&sb, &fork).unwrap();
        assert_eq!(dir.entries[0].offset, 0x1234);
    }

    #[test]
    fn rejects_short_form_on_a_non_directory() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[], false);
        let mut inode = dir_inode(&sb, 128, &fork);
        inode.mode = 0o100644; // regular file
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::NotADirectory)
        ));
    }

    #[test]
    fn rejects_short_form_on_a_non_local_fork() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[], false);
        let mut inode = dir_inode(&sb, 128, &fork);
        inode.format = Format::Extents;
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_larger_than_its_fork() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[("a", 1024, 1)], false);
        let mut inode = dir_inode(&sb, 128, &fork);
        inode.size = 4096;
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_header_that_does_not_fit() {
        let sb = sb_v5_ftype();
        let inode = dir_inode(&sb, 128, &[0u8; 3]);
        assert!(matches!(
            read_short_form(&inode, &[0u8, 0, 0], &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_entry_running_past_the_end() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("hello", 1024, 1)], false);
        fork[6] = 200; // claim a 200-byte name in a 15-byte directory
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_entry_with_empty_name() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("hello", 1024, 1)], false);
        fork[6] = 0;
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// The entries must land exactly on the declared size. This is what
    /// catches an entry measured with the wrong file type or inode width.
    #[test]
    fn rejects_short_form_with_trailing_bytes() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("hello", 1024, 1)], false);
        fork.push(0); // one byte the entries do not account for
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// Reading a with-ftype directory as though it had none consumes one
    /// byte too few per entry, so the walk ends in the wrong place.
    #[test]
    fn ftype_mismatch_is_caught_by_the_size_check() {
        let with = sb_v5_ftype();
        let without = sb_v5_noftype();
        let fork = build_sf(&with, 128, &[("hello", 1024, 1)], false);
        assert!(read_sf(&with, &fork).is_ok());
        assert!(
            read_sf(&without, &fork).is_err(),
            "a directory read with the wrong file-type assumption must not parse"
        );
    }

    /// The i8count is redundant with the inode numbers themselves, and
    /// XFS carries it precisely so a wrong-width read is detectable.
    #[test]
    fn rejects_short_form_with_wrong_i8count() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("hello", 1024, 1)], true);
        fork[offsets::sf_hdr::I8COUNT] = 2; // claims two; there are none
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_inode_above_the_56_bit_maximum() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("a", 1u64 << 40, 1)], true);
        // Set the top byte of the entry's inode number.
        let at = fork.len() - 8;
        fork[at] = 1;
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_inode_in_a_nonexistent_ag() {
        let sb = sb_v5_ftype();
        // agblklog 10 + inopblog 3, so ag 9 is well past agcount 4.
        let fork = build_sf(&sb, 128, &[("a", 9u64 << 13, 1)], false);
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_with_zero_inode_number() {
        let sb = sb_v5_ftype();
        let fork = build_sf(&sb, 128, &[("a", 0, 1)], false);
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_short_form_with_undefined_ftype() {
        let sb = sb_v5_ftype();
        let mut fork = build_sf(&sb, 128, &[("a", 1024, 1)], false);
        let at = fork.len() - 5; // the ftype byte, just before the inode
        fork[at] = 99;
        let inode = dir_inode(&sb, 128, &fork);
        assert!(matches!(
            read_short_form(&inode, &fork, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    // -- block form --------------------------------------------------

    #[test]
    fn parses_block_form_v5() {
        let sb = sb_v5_ftype();
        let buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1), ("beta", 2048, 2)]);
        let dir = parse_block_form(&buf, &sb).unwrap();
        assert_eq!(names(&dir.entries), [".", "..", "alpha", "beta"]);
        assert_eq!(dir.entries[0].ino, 128, "`.` names the directory itself");
        assert_eq!(dir.entries[2].ftype, Some(FileType::Regular));
        assert_eq!(dir.index.len(), 4);
        assert!(dir.index.iter().all(|e| !e.is_stale()));
    }

    #[test]
    fn parses_block_form_v4() {
        let sb = sb_v4_noftype();
        let buf = build_block_dir(&sb, 128, &[("alpha", 1024, 0)]);
        let dir = parse_block_form(&buf, &sb).unwrap();
        assert_eq!(names(&dir.entries), [".", "..", "alpha"]);
        assert!(dir.entries.iter().all(|e| e.ftype.is_none()));
    }

    /// Every entry's offset cookie is its own byte offset in the block,
    /// which is what its trailing tag repeats.
    #[test]
    fn block_form_entry_offsets_are_block_offsets() {
        let sb = sb_v5_ftype();
        let buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1)]);
        let dir = parse_block_form(&buf, &sb).unwrap();
        assert_eq!(dir.entries[0].offset, XFS_DIR3_DATA_HDR_SIZE as u32);
        for w in dir.entries.windows(2) {
            assert!(w[1].offset > w[0].offset);
        }
    }

    /// `parse_data_block` accepts the single-block magic and hands back
    /// the same entries.
    #[test]
    fn parse_data_block_accepts_block_form() {
        let sb = sb_v5_ftype();
        let buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1)]);
        let entries = parse_data_block(&buf, &sb).unwrap();
        assert_eq!(names(&entries), [".", "..", "alpha"]);
    }

    #[test]
    fn rejects_block_form_with_impossible_index_count() {
        let sb = sb_v5_ftype();
        let mut buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1)]);
        let tail = buf.len() - XFS_DIR2_BLOCK_TAIL_SIZE;
        buf[tail..tail + 4].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(matches!(
            parse_block_form(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_block_form_with_wrong_stale_count() {
        let sb = sb_v5_ftype();
        let mut buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1)]);
        let tail = buf.len() - XFS_DIR2_BLOCK_TAIL_SIZE;
        buf[tail + 4..tail + 8].copy_from_slice(&1u32.to_be_bytes());
        assert!(matches!(
            parse_block_form(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_block_form_with_unsorted_index() {
        let sb = sb_v5_ftype();
        let mut buf = build_block_dir(&sb, 128, &[("alpha", 1024, 1), ("beta", 2048, 1)]);
        let tail = buf.len() - XFS_DIR2_BLOCK_TAIL_SIZE;
        let count = be32(&buf, tail) as usize;
        let first = tail - count * XFS_DIR2_LEAF_ENTRY_SIZE;
        buf[first..first + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            parse_block_form(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    // -- data blocks -------------------------------------------------

    #[test]
    fn parses_data_block_v5() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 1), ("two", 2048, 7)]);
        let entries = parse_data_block(&buf, &sb).unwrap();
        assert_eq!(names(&entries), ["one", "two"]);
        assert_eq!(entries[1].ftype, Some(FileType::Symlink));
    }

    #[test]
    fn parses_data_block_v4() {
        let sb = sb_v4_noftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 0)]);
        let entries = parse_data_block(&buf, &sb).unwrap();
        assert_eq!(names(&entries), ["one"]);
    }

    /// An entirely free data block holds no entries and must not error.
    #[test]
    fn parses_empty_data_block() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[]);
        assert!(parse_data_block(&buf, &sb).unwrap().is_empty());
    }

    /// `XFS_DIR3_FT_UNKNOWN` and the whiteout type have no representable
    /// counterpart, so the caller is told to read the inode instead.
    #[test]
    fn unknown_and_whiteout_ftypes_become_none() {
        assert_eq!(ftype_from_raw(0).unwrap(), None);
        assert_eq!(ftype_from_raw(8).unwrap(), None);
        assert_eq!(ftype_from_raw(6).unwrap(), Some(FileType::Socket));
        assert!(ftype_from_raw(9).is_err());
    }

    #[test]
    fn rejects_data_block_with_wrong_magic() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// A v4 magic on a v5 filesystem means the block came from somewhere
    /// else entirely.
    #[test]
    fn rejects_data_block_from_the_wrong_on_disk_version() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        buf[0..4].copy_from_slice(&XFS_DIR2_DATA_MAGIC.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_entry_with_wrong_tag() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        let size = data_entry_size(3, true);
        let tag = XFS_DIR3_DATA_HDR_SIZE + size - 2;
        buf[tag..tag + 2].copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_entry_with_empty_name() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        buf[XFS_DIR3_DATA_HDR_SIZE + 8] = 0;
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_free_record_of_zero_length() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[]);
        buf[XFS_DIR3_DATA_HDR_SIZE + 2..XFS_DIR3_DATA_HDR_SIZE + 4]
            .copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_free_record_that_is_not_aligned() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[]);
        buf[XFS_DIR3_DATA_HDR_SIZE + 2..XFS_DIR3_DATA_HDR_SIZE + 4]
            .copy_from_slice(&12u16.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_free_record_with_wrong_back_tag() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[]);
        let end = buf.len();
        buf[end - 2..end].copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            parse_data_block(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_data_block_shorter_than_a_directory_block() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[]);
        assert!(matches!(
            parse_data_block(&buf[..1024], &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    // -- v5 header verification --------------------------------------

    #[test]
    fn verifies_a_good_data_block_header() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        verify_data_block(&buf, &sb, 42, 128).unwrap();
    }

    #[test]
    fn rejects_data_block_with_bad_crc() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        buf[XFS_DIR3_DATA_HDR_SIZE] ^= 0xFF;
        assert!(matches!(
            verify_data_block(&buf, &sb, 42, 128),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    /// A block that is internally perfect but was read from the wrong
    /// address. The checksum cannot catch this; the recorded address can.
    #[test]
    fn rejects_data_block_from_the_wrong_address() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        match verify_data_block(&buf, &sb, 43, 128) {
            Err(Error::BlockIdentityMismatch {
                expected, found, ..
            }) => {
                assert_eq!(expected, 43);
                assert_eq!(found, 42);
            }
            other => panic!("expected identity mismatch, got {other:?}"),
        }
    }

    /// A directory block belonging to a different directory.
    #[test]
    fn rejects_data_block_owned_by_another_inode() {
        let sb = sb_v5_ftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        match verify_data_block(&buf, &sb, 42, 999) {
            Err(Error::BlockIdentityMismatch {
                expected, found, ..
            }) => {
                assert_eq!(expected, 999);
                assert_eq!(found, 128);
            }
            other => panic!("expected identity mismatch, got {other:?}"),
        }
    }

    /// A block left behind by a previous filesystem on the same device.
    #[test]
    fn rejects_data_block_from_a_foreign_filesystem() {
        let sb = sb_v5_ftype();
        let mut buf = build_data_block(&sb, 128, &[("one", 1024, 1)]);
        buf[DIR3_BLK_HDR.uuid] ^= 0xFF;
        let crc = crc32c_with_zeroed_crc(&buf, DIR3_BLK_HDR.crc);
        buf[DIR3_BLK_HDR.crc..DIR3_BLK_HDR.crc + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            verify_data_block(&buf, &sb, 42, 128),
            Err(Error::BlockIdentityMismatch { .. })
        ));
    }

    /// v4 has no header to verify, so verification passes vacuously
    /// rather than inventing a failure.
    #[test]
    fn v4_blocks_have_nothing_to_verify() {
        let sb = sb_v4_noftype();
        let buf = build_data_block(&sb, 128, &[("one", 1024, 0)]);
        verify_data_block(&buf, &sb, 999, 999).unwrap();
    }

    #[test]
    fn verifies_a_good_index_block_header() {
        let sb = sb_v5_ftype();
        let buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 4, 0);
        verify_da_block(&buf, &sb, 42, 128).unwrap();
        assert!(matches!(
            verify_da_block(&buf, &sb, 43, 128),
            Err(Error::BlockIdentityMismatch { .. })
        ));
    }

    #[test]
    fn rejects_index_block_with_bad_crc() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 4, 0);
        buf[XFS_DIR3_LEAF_HDR_SIZE] ^= 0xFF;
        assert!(matches!(
            verify_da_block(&buf, &sb, 42, 128),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    // -- leaf form ---------------------------------------------------

    #[test]
    fn parses_leaf_block_v5() {
        let sb = sb_v5_ftype();
        let buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 5, 0);
        let leaf = parse_leaf(&buf, &sb).unwrap();
        assert_eq!(leaf.magic, XFS_DIR3_LEAF1_MAGIC);
        assert_eq!(leaf.count, 5);
        assert_eq!(leaf.stale, 0);
        assert_eq!(leaf.entries.len(), 5);
        assert_eq!(leaf.bestcount, Some(3));
        assert!(leaf.is_single_leaf());
    }

    #[test]
    fn parses_leaf_block_v4() {
        let sb = sb_v4_noftype();
        let buf = build_leaf(&sb, 128, XFS_DIR2_LEAF1_MAGIC, 3, 0);
        let leaf = parse_leaf(&buf, &sb).unwrap();
        assert_eq!(leaf.count, 3);
        assert!(leaf.is_single_leaf());
    }

    /// A node-form directory's leaves use a different magic and carry no
    /// best-free tail.
    #[test]
    fn parses_leafn_block() {
        let sb = sb_v5_ftype();
        let buf = build_leaf(&sb, 128, XFS_DIR3_LEAFN_MAGIC, 2, 0);
        let leaf = parse_leaf(&buf, &sb).unwrap();
        assert_eq!(leaf.magic, XFS_DIR3_LEAFN_MAGIC);
        assert!(!leaf.is_single_leaf());
        assert_eq!(leaf.bestcount, None);
    }

    #[test]
    fn counts_stale_leaf_entries() {
        let sb = sb_v5_ftype();
        let buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 5, 2);
        let leaf = parse_leaf(&buf, &sb).unwrap();
        assert_eq!(leaf.stale, 2);
        assert_eq!(leaf.entries.iter().filter(|e| e.is_stale()).count(), 2);
    }

    #[test]
    fn rejects_leaf_with_wrong_stale_count() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 5, 0);
        let counts = offsets::da_counts(XFS_DIR3_LEAF_HDR_SIZE, true);
        buf[counts + 2..counts + 4].copy_from_slice(&2u16.to_be_bytes());
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_leaf_with_unsorted_hashes() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 3, 0);
        buf[XFS_DIR3_LEAF_HDR_SIZE..XFS_DIR3_LEAF_HDR_SIZE + 4]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_leaf_with_too_many_entries() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 3, 0);
        let counts = offsets::da_counts(XFS_DIR3_LEAF_HDR_SIZE, true);
        buf[counts..counts + 2].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_leaf_with_impossible_bestcount() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 3, 0);
        let tail = buf.len() - XFS_DIR2_LEAF_TAIL_SIZE;
        buf[tail..tail + 4].copy_from_slice(&100_000u32.to_be_bytes());
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_leaf_with_a_node_magic() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, 1, 2);
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_leaf_from_the_wrong_on_disk_version() {
        let sb = sb_v5_ftype();
        let mut buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 3, 0);
        let m = offsets::da_blk::MAGIC;
        buf[m..m + 2].copy_from_slice(&XFS_DIR2_LEAF1_MAGIC.to_be_bytes());
        assert!(matches!(
            parse_leaf(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    // -- node form ---------------------------------------------------

    #[test]
    fn parses_node_block_v5() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, 1, 3);
        let node = parse_node(&buf, &sb).unwrap();
        assert_eq!(node.level, 1);
        assert_eq!(node.count, 3);
        assert_eq!(node.entries.len(), 3);
        assert_eq!(node.entries[0].hashval, 100);
        assert_eq!(node.entries[0].before, 10);
    }

    #[test]
    fn parses_node_block_v4() {
        let sb = sb_v4_noftype();
        let buf = build_node(&sb, 128, 1, 2);
        let node = parse_node(&buf, &sb).unwrap();
        assert_eq!(node.level, 1);
        assert_eq!(node.count, 2);
    }

    /// Descent picks the first child whose hash bound reaches the target.
    #[test]
    fn node_descends_to_the_first_covering_child() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, 1, 3); // bounds 100, 200, 300
        let node = parse_node(&buf, &sb).unwrap();
        assert_eq!(node.child_for_hash(50), Some(10));
        assert_eq!(node.child_for_hash(100), Some(10));
        assert_eq!(node.child_for_hash(101), Some(11));
        assert_eq!(node.child_for_hash(300), Some(12));
        assert_eq!(node.child_for_hash(301), None, "above every bound");
    }

    /// A node at level 0 would be a leaf; the two are told apart by this
    /// field as much as by the magic.
    #[test]
    fn rejects_node_at_level_zero() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, 0, 2);
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_node_deeper_than_the_format_allows() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, XFS_DA_NODE_MAXDEPTH + 1, 2);
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// A legal but deep B-tree is refused explicitly rather than walked
    /// on an untested guess.
    #[test]
    fn refuses_to_descend_a_deep_btree() {
        let sb = sb_v5_ftype();
        let buf = build_node(&sb, 128, MAX_SUPPORTED_NODE_LEVEL + 1, 2);
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn rejects_node_with_unsorted_hashes() {
        let sb = sb_v5_ftype();
        let mut buf = build_node(&sb, 128, 1, 3);
        buf[XFS_DA3_NODE_HDR_SIZE..XFS_DA3_NODE_HDR_SIZE + 4]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_node_with_too_many_children() {
        let sb = sb_v5_ftype();
        let mut buf = build_node(&sb, 128, 1, 3);
        let counts = offsets::da_counts(XFS_DA3_NODE_HDR_SIZE, true);
        buf[counts..counts + 2].copy_from_slice(&u16::MAX.to_be_bytes());
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_node_with_a_leaf_magic() {
        let sb = sb_v5_ftype();
        let buf = build_leaf(&sb, 128, XFS_DIR3_LEAF1_MAGIC, 2, 0);
        assert!(matches!(
            parse_node(&buf, &sb),
            Err(Error::BadSuperblock(_))
        ));
    }
}
