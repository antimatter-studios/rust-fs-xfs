//! Every on-disk structure an XFS directory is built out of, named.
//!
//! A directory is not one layout but five, and which one is in use is a
//! property of the *inode*, not of anything stamped on the directory:
//!
//! | Shape | Where the names live | How it is recognised |
//! |---|---|---|
//! | **short form** | inline in the inode's data fork | `di_format == local` |
//! | **block** | one directory block, names and index together | data magic `XDB3` / `XD2B` |
//! | **leaf** | several data blocks, one index block | data `XDD3` / `XD2D`, index `0x3df1` / `0xd2f1` |
//! | **node** | as leaf, index is a hash B-tree, free space in its own blocks | node `0x3ebe` / `0xfebe`, free `XDF3` / `XD2F` |
//! | **B+tree** | as node, but the *extent map* no longer fits the inode | `di_format == btree` |
//!
//! The last of those is worth stating plainly because the name misleads:
//! a "B+tree directory" contains no directory structure a node-form
//! directory does not. What changed is the inode's block map, which
//! moved from an inline extent list into a `bmbt`. Every block reached
//! through it is one of the four shapes above. See [`crate::bmbt`] for
//! the map, and treat this module as complete for the directory itself.
//!
//! # What this module is
//!
//! A reference. It is offsets, sizes, magic numbers, sentinels and
//! limits, and nothing else: no reads, no parsing, no dependency on the
//! rest of the crate. `src/dir.rs` is the parser and remains the working
//! code; where it already names a value, the name and the value here are
//! the same one, deliberately, so the two can be compared by eye.
//!
//! Most of what is below is not called by anything. That is the point.
//! An offset is only checkable against the format documentation when its
//! neighbours are named too: `owner` at 40 in a v5 block header is only
//! obviously right when `blkno` at 8, `lsn` at 16 and the 16-byte `uuid`
//! at 24 are visible above it to be counted off against. The next person
//! to extend the directory code should be reading a table, not
//! rediscovering one.
//!
//! # Byte order
//!
//! Big-endian, everywhere, as in the whole of XFS. The one exception is
//! a CRC field, which is little-endian; each structure below says so
//! again at the field itself, because it is the single easiest thing to
//! get wrong here.
//!
//! # The traps, collected
//!
//! Each is repeated at the structure it applies to. Together they are
//! most of what makes directories harder than they look.
//!
//! 1. **A directory block is not a filesystem block.** Its size is
//!    `sb_blocksize << sb_dirblklog` and it can be up to 64 KiB. Every
//!    "block" below means a directory block unless it says otherwise.
//! 2. **Three different block units are in play at once**: filesystem
//!    blocks (extent `startoff`, and `xfs_dablk_t`, which is what a
//!    B-tree node's `before` holds), directory blocks (`xfs_dir2_db_t`,
//!    which is what a free-index block's `firstdb` counts), and 512-byte
//!    basic blocks (the `blkno` a v5 header records about itself). All
//!    three coincide only on a 512-byte filesystem with `sb_dirblklog`
//!    of 0, and the first two coincide whenever `sb_dirblklog` is 0,
//!    which is the default. So a value that looks right on the fixture
//!    to hand is weak evidence, and only a filesystem with a directory
//!    block larger than its filesystem block tells the units apart.
//! 3. **The file-type byte moves the fields after it.** When the feature
//!    is on, one byte sits between a name and whatever follows it, which
//!    is the inode number in a short-form entry and the tag in a data
//!    block entry. Reading it wrong shifts every subsequent field.
//! 4. **The feature is advertised in two places**, depending on the
//!    on-disk version, and a v4 filesystem from any recent `mkfs` has it
//!    on. See [`XFS_SB_FEAT_INCOMPAT_FTYPE`] and
//!    [`XFS_SB_VERSION2_FTYPE`].
//! 5. **A short-form inode number's width is a property of the
//!    directory, not of the entry.** `i8count` is a count, not a flag,
//!    and when it is non-zero every inode number in the directory is
//!    eight bytes wide, the parent's included.
//! 6. **Tags are at the end of the record, not at a fixed offset.** Both
//!    a used entry and a free region repeat their own start offset in
//!    their last two bytes, and how far away that is depends on the
//!    record's own length.
//! 7. **A leaf and a B-tree node put different fields in the same two
//!    slots.** After the shared block-info header, a leaf has `count`
//!    then `stale`; a node has `count` then `level`. Only the magic
//!    tells them apart.
//! 8. **v5 headers pad.** Both the leaf and the node header end with
//!    four bytes of padding after that pair, so the records start at the
//!    header size, not eight bytes past the pair. [`offsets::da_counts`]
//!    exists so that arithmetic is written once.
//! 9. **The two `bests` arrays disagree about which way they grow.** A
//!    leaf-form directory's array ends at the block's tail and its
//!    region extends *downwards* as data blocks are added; a free-index
//!    block's array starts immediately after the header and extends
//!    upwards. In both, index 0 is at the array's lowest address.

// Nothing here is called yet, and most of it never will be directly:
// this module is a table to read, and a table with holes in it cannot be
// checked against the format documentation. `dead_code` would fire on
// almost every item the moment it is declared as a private module.
#![allow(dead_code)]

// ---------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------

/// Every record in a directory data block, used or free, starts on an
/// 8-byte boundary and occupies a whole multiple of 8 bytes.
///
/// This is `XFS_DIR2_DATA_ALIGN`, and it is also the unit a
/// `xfs_dir2_dataptr_t` counts in, which is why a directory can address
/// a 32 GiB data space with a 32-bit value.
pub const XFS_DIR2_DATA_ALIGN: usize = 8;

/// [`XFS_DIR2_DATA_ALIGN`] as a shift.
pub const XFS_DIR2_DATA_ALIGN_LOG: u32 = 3;

/// The largest a directory block may be. The specification states the
/// limit directly, and it is why every offset and length inside a data
/// block fits in a `u16`.
pub const XFS_DIR2_MAX_DIRBLOCK_SIZE: usize = 65_536;

/// The longest name a directory entry can hold, imposed by `namelen`
/// being a single byte. The specification does not state a separate
/// directory limit; this is the field's own bound.
pub const XFS_DIR2_MAX_NAMELEN: usize = 255;

/// How deep a directory or attribute hash B-tree may be
/// (`XFS_DA_NODE_MAXDEPTH`). Leaves are level 0, so an interior node's
/// level lies in `1..=5`.
pub const XFS_DA_NODE_MAXDEPTH: u16 = 5;

// ---------------------------------------------------------------------
// The three address spaces of a directory
// ---------------------------------------------------------------------
//
// A directory's blocks are not laid out one after another. The file is
// carved into three spaces 32 GiB apart, and the extent map for any
// directory past block form therefore has two enormous holes in it that
// are not corruption.

/// How far apart the directory's three address spaces are placed, in
/// bytes: 32 GiB.
///
/// This is a *byte* offset into the directory's file address space.
/// Converting it into blocks needs the right block size, and which one
/// is right depends on what the number will be compared against. See
/// [`leaf_first_fsb`] and [`leaf_first_db`].
pub const XFS_DIR2_SPACE_SIZE: u64 = 1 << 35;

/// Space 0: the data blocks, which hold the names. It starts at file
/// offset zero, and the first data block must always be present. A
/// directory with a hole at its start is corrupt, because block 0 holds
/// `.` and `..`.
pub const XFS_DIR2_DATA_SPACE: u64 = 0;

/// Space 1: the hash index. In leaf form this is a single block; in node
/// form it is a B-tree of them.
pub const XFS_DIR2_LEAF_SPACE: u64 = 1;

/// Space 2: the free-space index, which only node-form and larger
/// directories have.
pub const XFS_DIR2_FREE_SPACE: u64 = 2;

/// Byte offset at which the data space begins: zero.
pub const XFS_DIR2_DATA_OFFSET: u64 = XFS_DIR2_DATA_SPACE * XFS_DIR2_SPACE_SIZE;

/// Byte offset at which the hash index begins: 32 GiB.
pub const XFS_DIR2_LEAF_OFFSET: u64 = XFS_DIR2_LEAF_SPACE * XFS_DIR2_SPACE_SIZE;

/// Byte offset at which the free-space index begins: 64 GiB.
pub const XFS_DIR2_FREE_OFFSET: u64 = XFS_DIR2_FREE_SPACE * XFS_DIR2_SPACE_SIZE;

/// The filesystem-block offset at which the hash index begins, which is
/// what an extent record's `startoff` will hold for it.
///
/// On a 4 KiB filesystem this is 8388608, the number that appears in
/// every worked example in the specification. It is *not* affected by
/// `sb_dirblklog`: extent offsets count filesystem blocks whatever the
/// directory block size is.
pub const fn leaf_first_fsb(blocksize: u64) -> u64 {
    XFS_DIR2_LEAF_OFFSET / blocksize
}

/// The filesystem-block offset at which the free-space index begins.
pub const fn free_first_fsb(blocksize: u64) -> u64 {
    XFS_DIR2_FREE_OFFSET / blocksize
}

/// The *directory*-block number of the first block of the hash index
/// (`XFS_DIR2_LEAF_FIRSTDB`).
///
/// Divided by the directory block size, not the filesystem block size,
/// because this is an `xfs_dir2_db_t`. On a filesystem with 4 KiB blocks
/// and 16 KiB directory blocks this is a quarter of [`leaf_first_fsb`],
/// and using one where the other belongs is the whole reason both are
/// spelled out here.
pub const fn leaf_first_db(dirblocksize: u64) -> u64 {
    XFS_DIR2_LEAF_OFFSET / dirblocksize
}

/// The directory-block number of the first free-index block.
pub const fn free_first_db(dirblocksize: u64) -> u64 {
    XFS_DIR2_FREE_OFFSET / dirblocksize
}

// ---------------------------------------------------------------------
// Magic numbers
// ---------------------------------------------------------------------
//
// Two families, and they are not the same width. A data-block magic is a
// `u32` of four ASCII characters at offset 0. A leaf or node magic is a
// `u16` at offset 8, because the block starts with the two sibling
// pointers instead. Reading either at the other's offset finds nothing
// recognisable, which at least fails loudly.

/// `XD2B` — a v4 block-form directory: the whole directory in one block.
pub const XFS_DIR2_BLOCK_MAGIC: u32 = 0x5844_3242;
/// `XDB3` — a v5 block-form directory.
pub const XFS_DIR3_BLOCK_MAGIC: u32 = 0x5844_4233;

/// `XD2D` — a v4 directory data block, one of several holding names.
pub const XFS_DIR2_DATA_MAGIC: u32 = 0x5844_3244;
/// `XDD3` — a v5 directory data block.
pub const XFS_DIR3_DATA_MAGIC: u32 = 0x5844_4433;

/// `XD2F` — a v4 free-space index block, which only node-form and
/// larger directories have.
pub const XFS_DIR2_FREE_MAGIC: u32 = 0x5844_3246;
/// `XDF3` — a v5 free-space index block.
pub const XFS_DIR3_FREE_MAGIC: u32 = 0x5844_4633;

/// v4 combined leaf-and-free-space block: the sole index block of a
/// leaf-form directory, the one shape that carries a `bests` array in
/// its own tail.
pub const XFS_DIR2_LEAF1_MAGIC: u16 = 0xd2f1;
/// v5 combined leaf-and-free-space block.
pub const XFS_DIR3_LEAF1_MAGIC: u16 = 0x3df1;

/// v4 leaf block hanging off a node-form directory's B-tree. Same layout
/// as a leaf1 block, but with no `bests` array and no tail: once there
/// is more than one leaf, no single leaf can hold the free-space
/// summary, which is why the free-index space exists at all.
pub const XFS_DIR2_LEAFN_MAGIC: u16 = 0xd2ff;
/// v5 leaf block of a node-form directory.
pub const XFS_DIR3_LEAFN_MAGIC: u16 = 0x3dff;

/// v4 interior node of a directory or attribute hash B-tree.
pub const XFS_DA_NODE_MAGIC: u16 = 0xfebe;
/// v5 interior node of a directory or attribute hash B-tree.
pub const XFS_DA3_NODE_MAGIC: u16 = 0x3ebe;

// ---------------------------------------------------------------------
// Sentinels
// ---------------------------------------------------------------------

/// Marks the start of a free region inside a data block, standing where
/// the top two bytes of an inode number would be
/// (`XFS_DIR2_DATA_FREE_TAG`).
///
/// It is unambiguous because a real inode number can never begin
/// `0xffff`: XFS caps inode numbers at 56 bits, so the top byte of an
/// on-disk inode number is always zero.
pub const XFS_DIR2_DATA_FREE_TAG: u16 = 0xffff;

/// An index record whose address is this has had its name removed but
/// has not been compacted away yet (`XFS_DIR2_NULL_DATAPTR`). The
/// header's `stale` count says how many such records to expect, and a
/// disagreement between the two is a good corruption signal.
pub const XFS_DIR2_NULL_DATAPTR: u32 = 0;

/// A `bests` entry of this value means the data block it describes is
/// not present at all (`NULLDATAOFF`): every name in it was deleted and
/// the block was freed, leaving a hole in the directory's extent map.
///
/// Confirmed by observation, not only by the specification's prose. A
/// directory whose middle data blocks were emptied showed `nvalid` of 24
/// against `nused` of 20, with exactly four `bests` entries set to
/// `0xffff` and a four-block hole in the extent map.
pub const XFS_DIR2_DATA_FREE_NULL: u16 = 0xffff;

/// Largest inode number a short-form directory can hold in four bytes
/// (`XFS_DIR2_MAX_SHORT_INUM`). One entry above it converts the whole
/// directory to the eight-byte representation.
pub const XFS_DIR2_MAX_SHORT_INUM: u64 = 0xffff_ffff;

/// XFS caps inode numbers at 56 bits (`XFS_MAXINUMBER`), which is what
/// makes [`XFS_DIR2_DATA_FREE_TAG`] safe to overlay on one.
pub const XFS_MAXINUMBER: u64 = (1u64 << 56) - 1;

/// A leaf or node block with no sibling at its own level stores zero in
/// the pointer, not the all-bits-set value the specification gives as
/// XFS's general on-disk null.
///
/// Zero is safe here because block 0 of the directory's address space is
/// a data block and can never be a leaf or a node, so it is not a
/// possible sibling. Applying the general convention instead, and
/// waiting for an all-ones terminator, follows the chain into block 0
/// and finds a data magic where a leaf magic should be. Both the
/// specification's worked examples and a v5 filesystem built for this
/// module show plain zeroes.
pub const XFS_DA_NULL_SIBLING: u32 = 0;

// ---------------------------------------------------------------------
// The file-type byte
// ---------------------------------------------------------------------

/// `XFS_SB_FEAT_INCOMPAT_FTYPE` — where a **v5** filesystem advertises
/// that directory entries carry a file-type byte.
pub const XFS_SB_FEAT_INCOMPAT_FTYPE: u32 = 1 << 0;

/// `XFS_SB_VERSION2_FTYPE` — where a **v4** filesystem advertises the
/// same feature, in `sb_features2`, and only meaningful when
/// `XFS_SB_VERSION_MOREBITSBIT` is set in `sb_versionnum`.
///
/// A v4 filesystem from any recent `mkfs` has this set, so treating the
/// v5 bit as the whole test reads every entry on such a filesystem one
/// byte short and shifts the inode number that follows the name.
pub const XFS_SB_VERSION2_FTYPE: u32 = 0x0000_0200;

/// File type is not recorded; the caller must read the inode.
pub const XFS_DIR3_FT_UNKNOWN: u8 = 0;
/// A regular file.
pub const XFS_DIR3_FT_REG_FILE: u8 = 1;
/// A directory.
pub const XFS_DIR3_FT_DIR: u8 = 2;
/// A character device.
pub const XFS_DIR3_FT_CHRDEV: u8 = 3;
/// A block device.
pub const XFS_DIR3_FT_BLKDEV: u8 = 4;
/// A named pipe.
pub const XFS_DIR3_FT_FIFO: u8 = 5;
/// A socket.
pub const XFS_DIR3_FT_SOCK: u8 = 6;
/// A symbolic link.
pub const XFS_DIR3_FT_SYMLINK: u8 = 7;
/// An overlay filesystem whiteout, which has no counterpart in a
/// conventional file type.
pub const XFS_DIR3_FT_WHT: u8 = 8;
/// One past the last defined type. A value at or above this is not a
/// file type at all, and is good evidence the entry was read at the
/// wrong offset.
pub const XFS_DIR3_FT_MAX: u8 = 9;

// ---------------------------------------------------------------------
// Structure sizes
// ---------------------------------------------------------------------
//
// Named once, here, and referred to by name from the offset tables
// below. The `XFS_DIR2_*` sizes are v4 and the `XFS_DIR3_*` ones v5;
// where a structure is unchanged between versions there is only one.

/// `xfs_dir3_blk_hdr` — the v5 self-describing prefix on every data,
/// block-form and free-index block. 48 bytes, and there is no v4
/// equivalent: a v4 data block begins with its magic and nothing else.
pub const XFS_DIR3_BLK_HDR_SIZE: usize = 48;

/// `xfs_da_blkinfo` — the v4 prefix on every leaf and node block.
pub const XFS_DA_BLKINFO_SIZE: usize = 12;

/// `xfs_da3_blkinfo` — the v5 prefix on every leaf and node block. The
/// first twelve bytes are `xfs_da_blkinfo` unchanged, so the sibling
/// pointers and the magic are at the same offsets in both versions.
pub const XFS_DA3_BLKINFO_SIZE: usize = 56;

/// `xfs_dir2_data_free` — one "best free" record: an offset and a
/// length, both `u16`.
pub const XFS_DIR2_DATA_FREE_SIZE: usize = 4;

/// How many "best free" records a data block header carries
/// (`XFS_DIR2_DATA_FD_COUNT`): the three largest free regions in the
/// block, largest first, with unused slots zeroed.
pub const XFS_DIR2_DATA_FD_COUNT: usize = 3;

/// `xfs_dir2_data_hdr` — a v4 data or block-form header: magic plus
/// three best-free records.
pub const XFS_DIR2_DATA_HDR_SIZE: usize = 16;

/// `xfs_dir3_data_hdr` — a v5 data or block-form header: the 48-byte
/// self-describing header, three best-free records, and four bytes of
/// padding to bring the entries back onto an 8-byte boundary.
pub const XFS_DIR3_DATA_HDR_SIZE: usize = 64;

/// `xfs_dir2_leaf_hdr` — a v4 leaf header: block info, count, stale.
pub const XFS_DIR2_LEAF_HDR_SIZE: usize = 16;

/// `xfs_dir3_leaf_hdr` — a v5 leaf header: block info, count, stale, and
/// four bytes of padding.
pub const XFS_DIR3_LEAF_HDR_SIZE: usize = 64;

/// `xfs_da_node_hdr` — a v4 node header: block info, count, level.
/// Identical in size and shape to a v4 leaf header, with `level` where
/// the leaf keeps `stale`.
pub const XFS_DA_NODE_HDR_SIZE: usize = 16;

/// `xfs_da3_node_hdr` — a v5 node header: block info, count, level, and
/// four bytes of padding.
pub const XFS_DA3_NODE_HDR_SIZE: usize = 64;

/// `xfs_dir2_free_hdr` — a v4 free-index header: magic, `firstdb`,
/// `nvalid`, `nused`.
pub const XFS_DIR2_FREE_HDR_SIZE: usize = 16;

/// `xfs_dir3_free_hdr` — a v5 free-index header: the 48-byte
/// self-describing header, the same three counts, and four bytes of
/// padding.
///
/// Established by observation rather than from the specification, which
/// predates v5 entirely: a v5 free-index block's `bests[23]` was found
/// at byte 0x6e, which places `bests[0]` at 64.
pub const XFS_DIR3_FREE_HDR_SIZE: usize = 64;

/// `xfs_dir2_leaf_entry` — one hash index record: a hash and an address.
/// The same in both versions, and the same in a leaf block as in the
/// tail of a block-form directory.
pub const XFS_DIR2_LEAF_ENTRY_SIZE: usize = 8;

/// `xfs_da_node_entry` — one child record of a B-tree node: a hash bound
/// and a block number. The same in both versions.
pub const XFS_DA_NODE_ENTRY_SIZE: usize = 8;

/// `xfs_dir2_block_tail` — the last eight bytes of a block-form
/// directory: the index record count and the stale count.
pub const XFS_DIR2_BLOCK_TAIL_SIZE: usize = 8;

/// `xfs_dir2_leaf_tail` — the last four bytes of a leaf-form directory's
/// index block: the length of the `bests` array that precedes it.
pub const XFS_DIR2_LEAF_TAIL_SIZE: usize = 4;

/// One entry of either `bests` array: a `u16` holding one data block's
/// `bestfree[0].length`.
pub const XFS_DIR2_BEST_SIZE: usize = 2;

/// `xfs_dir2_sf_hdr` when the directory's inode numbers fit in four
/// bytes: two counts and a 4-byte parent.
pub const XFS_DIR2_SF_HDR_SIZE_4: usize = 6;

/// `xfs_dir2_sf_hdr` when they do not: two counts and an 8-byte parent.
pub const XFS_DIR2_SF_HDR_SIZE_8: usize = 10;

/// The smallest a used entry in a data block can be: an inode number, a
/// name length, one byte of name, and the 2-byte tag, rounded up to
/// [`XFS_DIR2_DATA_ALIGN`]. The file-type byte does not change it,
/// because the rounding absorbs it.
pub const DATA_ENTRY_MIN_SIZE: usize = 16;

/// Both `.` and `..` occupy exactly this much in a data block, whether
/// or not the file-type feature is on: one is 12 or 13 bytes before
/// alignment and the other 13 or 14, and all four round to 16.
///
/// So the first real name in a block-form directory, or in data block 0
/// of any larger one, always begins 32 bytes past the header. It is the
/// cheapest sanity check there is on a data block header size.
pub const XFS_DIR2_DOT_ENTRY_SIZE: usize = 16;

// ---------------------------------------------------------------------
// Offsets
// ---------------------------------------------------------------------

/// Byte offsets within the on-disk directory structures, one submodule
/// per structure.
///
/// Directories are the worst case in XFS for unnamed literals: fifteen
/// distinct structures, five of them in a v4 and a v5 shape that differ
/// only by a header prefix, and two of them putting different fields at
/// the same offset.
///
/// Every submodule states its own byte order and its own size. Fields
/// nothing reads are named alongside the ones that are read, because an
/// offset in isolation cannot be checked and an offset with its
/// neighbours can.
pub mod offsets {
    /// `xfs_dir3_blk_hdr` — the v5 self-describing prefix on every data,
    /// block-form and free-index block.
    ///
    /// There is no v4 counterpart. A v4 data block starts straight in
    /// with its magic, so every field of a v4 data header sits 48 bytes
    /// earlier than the v5 one.
    ///
    /// Big-endian, except `CRC`.
    pub mod dir3_blk {
        /// `u32`. Which kind of block this is, and which on-disk version
        /// wrote it.
        pub const MAGIC: usize = 0;
        /// `u32`, **little-endian**. CRC32C over the whole directory
        /// block with this field taken as zero, not over the header
        /// alone.
        pub const CRC: usize = 4;
        /// `u64`. Where the block believes it lives, in 512-byte basic
        /// blocks. Not a filesystem block number: a block at filesystem
        /// block 10 on a 4 KiB filesystem records 80 here. Comparing it
        /// against a filesystem block number makes every valid block
        /// look misdirected.
        pub const BLKNO: usize = 8;
        /// `u64`. Sequence number of the log record that last wrote the
        /// block, as a packed (cycle, block) pair.
        pub const LSN: usize = 16;
        /// 16 raw bytes. The filesystem this block belongs to,
        /// `sb_meta_uuid` rather than `sb_uuid` when the metadata-UUID
        /// feature is on.
        pub const UUID: usize = 24;
        /// `u64`. Inode number of the directory that owns the block.
        /// Together with `BLKNO` and `UUID` this is what catches an
        /// intact block that came from somewhere else, which a checksum
        /// alone cannot.
        pub const OWNER: usize = 40;
        /// Size of the prefix; the structure that carries it continues
        /// from here.
        pub const SIZE: usize = super::super::XFS_DIR3_BLK_HDR_SIZE;
    }

    /// `xfs_da_blkinfo` and its v5 extension `xfs_da3_blkinfo` — the
    /// prefix on every leaf and B-tree node block, shared with extended
    /// attributes.
    ///
    /// Both versions are described here because v5 is a strict extension
    /// of v4: the first twelve bytes are identical, so the sibling
    /// pointers and the magic can be read before knowing which version
    /// wrote the block. Everything from `CRC` onwards is v5 only.
    ///
    /// Big-endian, except `CRC`.
    pub mod da_blk {
        /// `u32`. Next block in the sibling chain at this level, or 0
        /// for none. In a node-form directory the leaves are threaded
        /// together by these, and the end leaves of adjacent nodes point
        /// at each other, so the chain crosses parents.
        pub const FORW: usize = 0;
        /// `u32`. Previous block in the chain, or 0 for none.
        pub const BACK: usize = 4;
        /// `u16`. Which kind of index block this is. Note the width: a
        /// data block's magic is a `u32` at offset 0, this one is a
        /// `u16` at offset 8.
        pub const MAGIC: usize = 8;
        /// `u16`. Padding, to align what follows.
        pub const PAD: usize = 10;
        /// `u32`, **little-endian**, v5 only. CRC32C over the whole
        /// directory block with this field taken as zero.
        pub const CRC: usize = 12;
        /// `u64`, v5 only. The block's own address in 512-byte basic
        /// blocks, as in [`super::dir3_blk::BLKNO`].
        pub const BLKNO: usize = 16;
        /// `u64`, v5 only. Log sequence number of the last write.
        pub const LSN: usize = 24;
        /// 16 raw bytes, v5 only. The owning filesystem.
        pub const UUID: usize = 32;
        /// `u64`, v5 only. Inode number of the owning directory.
        pub const OWNER: usize = 48;
        /// Size of the v4 prefix.
        pub const SIZE_V4: usize = super::super::XFS_DA_BLKINFO_SIZE;
        /// Size of the v5 prefix.
        pub const SIZE_V5: usize = super::super::XFS_DA3_BLKINFO_SIZE;
    }

    /// `xfs_dir2_sf_hdr` — the header of a short-form directory, stored
    /// inline in the inode's data fork.
    ///
    /// Big-endian, and packed: there is no alignment anywhere in a
    /// short-form directory. The parent inode number begins two bytes
    /// in, which is not a multiple of its own width in either
    /// representation, so it has to be read byte-wise.
    pub mod sf_hdr {
        /// `u8`. Number of entries, which excludes `.` and `..`. Short
        /// form stores neither: `.` is the directory's own inode number
        /// and `..` is the `PARENT` field below.
        pub const COUNT: usize = 0;
        /// `u8`. How many of this directory's inode numbers, the
        /// parent's included, need more than 32 bits.
        ///
        /// This is a **count, not a flag**. When it is non-zero, *every*
        /// inode number in the directory is stored in eight bytes, not
        /// just the wide ones, and XFS rewrites the whole directory when
        /// the count crosses zero in either direction. Treating it as a
        /// per-entry marker misreads every entry after the first wide
        /// one.
        pub const I8COUNT: usize = 1;
        /// Inode number of the parent directory, four bytes wide when
        /// `I8COUNT` is zero and eight when it is not.
        pub const PARENT: usize = 2;
        /// Header size when inode numbers are four bytes.
        pub const SIZE_4: usize = super::super::XFS_DIR2_SF_HDR_SIZE_4;
        /// Header size when they are eight.
        pub const SIZE_8: usize = super::super::XFS_DIR2_SF_HDR_SIZE_8;
    }

    /// `xfs_dir2_sf_entry` — one entry of a short-form directory.
    ///
    /// Variable length and completely unaligned: entries are packed end
    /// to end with the remaining space in the fork zeroed, and the whole
    /// run must end exactly at the inode's `di_size`. Anything left over
    /// means an entry was measured wrongly, and the file-type byte and
    /// the inode number width are the two ways to get that wrong.
    ///
    /// Big-endian.
    pub mod sf_entry {
        /// `u8`. Length of the name, with no terminator and no
        /// requirement to be valid UTF-8.
        pub const NAMELEN: usize = 0;
        /// `u16` at an odd offset. The entry's directory offset cookie,
        /// which is what a `readdir` position is built from.
        ///
        /// It points at nothing in the inode. It is the byte offset the
        /// entry *would* occupy in the data block this directory will
        /// become when it outgrows the inode, which is why the first
        /// entry's cookie is 0x30 on v4 and 0x60 on v5: past the
        /// header, past `.`, past `..`. Both the specification's
        /// worked example and a v5 filesystem built for this module
        /// show exactly those values.
        pub const OFFSET: usize = 1;
        /// The name itself, `NAMELEN` bytes.
        pub const NAME: usize = 3;
        /// The file type byte, present only when the filesystem has the
        /// feature. It sits between the name and the inode number, which
        /// is why omitting it shifts the inode number one byte earlier
        /// and produces a plausible but wrong listing.
        pub const fn ftype(namelen: usize) -> usize {
            NAME + namelen
        }
        /// The entry's inode number, four or eight bytes wide according
        /// to the header's `i8count`.
        pub const fn inumber(namelen: usize, has_ftype: bool) -> usize {
            NAME + namelen + if has_ftype { 1 } else { 0 }
        }
        /// Total size of one entry.
        pub const fn size(namelen: usize, has_ftype: bool, wide_ino: bool) -> usize {
            inumber(namelen, has_ftype) + if wide_ino { 8 } else { 4 }
        }
    }

    /// `xfs_dir2_data_hdr` (v4) and `xfs_dir3_data_hdr` (v5) — the head
    /// of a data block or of a block-form directory.
    ///
    /// The two versions carry the same three "best free" records; v5
    /// puts the 48-byte self-describing header in front of them and four
    /// bytes of padding behind, so everything shifts by 48 and the
    /// entries still start 8-byte aligned.
    ///
    /// Big-endian.
    pub mod data_hdr {
        /// `u32`. v4 only, and at offset 0 because a v4 block has no
        /// self-describing header. On v5 the magic is
        /// [`super::dir3_blk::MAGIC`], which is also offset 0, so a
        /// magic can be read without knowing the version first.
        pub const V4_MAGIC: usize = 0;
        /// Start of the three-record best-free array on v4.
        pub const V4_BESTFREE: usize = 4;
        /// Size of the v4 header, and so the offset of its first entry.
        pub const V4_SIZE: usize = super::super::XFS_DIR2_DATA_HDR_SIZE;
        /// Start of the best-free array on v5, immediately after the
        /// self-describing header.
        pub const V5_BESTFREE: usize = super::dir3_blk::SIZE;
        /// `u32`. Padding on v5, so that entries begin on an 8-byte
        /// boundary. Nothing reads it, and it is named because the 64
        /// below is only obviously right with it counted in.
        pub const V5_PAD: usize = V5_BESTFREE + 3 * super::super::XFS_DIR2_DATA_FREE_SIZE;
        /// Size of the v5 header, and so the offset of its first entry.
        pub const V5_SIZE: usize = super::super::XFS_DIR3_DATA_HDR_SIZE;
        /// Offset of best-free record `i`, given the header's start.
        pub const fn bestfree(bestfree_start: usize, i: usize) -> usize {
            bestfree_start + i * super::super::XFS_DIR2_DATA_FREE_SIZE
        }
    }

    /// `xfs_dir2_data_free` — one record of the best-free array: one of
    /// the three largest free regions in this data block, sorted largest
    /// first, with unused slots left as zeroes.
    ///
    /// It is a cache and not the truth. The free regions themselves are
    /// the `xfs_dir2_data_unused` records in the block; this array only
    /// saves scanning for them when allocating a new entry, and
    /// `bestfree[0].length` is additionally what both `bests` arrays
    /// summarise for the block.
    ///
    /// Big-endian. Both fields are `u16` because a directory block
    /// cannot exceed 64 KiB.
    pub mod data_free {
        /// `u16`. Byte offset of the free region from the start of the
        /// block. The bytes at that offset must be an
        /// `xfs_dir2_data_unused` record.
        pub const OFFSET: usize = 0;
        /// `u16`. Length of the free region in bytes.
        pub const LENGTH: usize = 2;
        /// Size of one record.
        pub const SIZE: usize = super::super::XFS_DIR2_DATA_FREE_SIZE;
    }

    /// `xfs_dir2_data_entry` — one used entry in a directory data block.
    ///
    /// Variable length, rounded up to [`crate::format::dir::XFS_DIR2_DATA_ALIGN`],
    /// and ending in a tag that repeats the entry's own offset within
    /// the block. That redundancy is the cheapest available detector for
    /// a walk that has lost alignment, and it is worth checking on every
    /// record rather than only when something already looks wrong.
    ///
    /// Big-endian.
    pub mod data_entry {
        /// `u64`. The entry's inode number, always eight bytes here.
        /// Only short form has the narrow representation.
        pub const INUMBER: usize = 0;
        /// `u8`. Length of the name.
        pub const NAMELEN: usize = 8;
        /// The name itself, `NAMELEN` bytes.
        pub const NAME: usize = 9;
        /// The file type byte, when the filesystem has the feature. As
        /// in short form it sits directly after the name, and here it
        /// pushes the tag rather than the inode number.
        pub const fn ftype(namelen: usize) -> usize {
            NAME + namelen
        }
        /// `u16`. The entry's own byte offset within the block,
        /// repeated. It is at the **end** of the padded record, so its
        /// position depends on the record's length and not on the name
        /// length alone.
        pub const fn tag(entry_size: usize) -> usize {
            entry_size - 2
        }
        /// Total size of one entry, padded to the 8-byte alignment.
        /// Never smaller than
        /// [`super::super::DATA_ENTRY_MIN_SIZE`].
        pub const fn size(namelen: usize, has_ftype: bool) -> usize {
            let align = super::super::XFS_DIR2_DATA_ALIGN;
            let raw = NAME + namelen + if has_ftype { 1 } else { 0 } + 2;
            (raw + align - 1) & !(align - 1)
        }
    }

    /// `xfs_dir2_data_unused` — one free region in a directory data
    /// block, sharing the space with used entries and told apart from
    /// one by its first two bytes.
    ///
    /// Big-endian.
    pub mod data_unused {
        /// `u16`. [`super::super::XFS_DIR2_DATA_FREE_TAG`], standing
        /// where a used entry keeps the top of its inode number.
        pub const FREETAG: usize = 0;
        /// `u16`. Total length of the free region, always a multiple of
        /// 8. This is the record's own size, not a payload length.
        pub const LENGTH: usize = 2;
        /// `u16`. The region's own start offset, repeated at
        /// `LENGTH - 2` from the start of the region. Deleting an entry
        /// leaves this tag exactly where it already was, which is why a
        /// freshly freed region's tag still reads correctly.
        pub const fn tag(length: usize) -> usize {
            length - 2
        }
    }

    /// `xfs_dir2_block_tail` — the last eight bytes of a block-form
    /// directory, at `dirblocksize - 8`.
    ///
    /// A block-form directory is a data block with its hash index packed
    /// into the same block: entries grow forwards from the header, the
    /// index grows backwards from this tail, and the free region in the
    /// middle shrinks from both ends.
    ///
    /// Big-endian.
    pub mod block_tail {
        /// `u32`. Number of hash index records, stale ones included.
        pub const COUNT: usize = 0;
        /// `u32`. How many of those records are stale.
        pub const STALE: usize = 4;
        /// Size of the tail.
        pub const SIZE: usize = super::super::XFS_DIR2_BLOCK_TAIL_SIZE;
        /// Where the tail starts in a block of the given size.
        pub const fn at(dirblocksize: usize) -> usize {
            dirblocksize - SIZE
        }
        /// Where the index records start, given how many there are.
        /// They end where the tail begins.
        pub const fn index_start(dirblocksize: usize, count: usize) -> usize {
            at(dirblocksize) - count * super::super::XFS_DIR2_LEAF_ENTRY_SIZE
        }
    }

    /// `xfs_dir2_leaf_hdr` (v4) and `xfs_dir3_leaf_hdr` (v5) — the head
    /// of a leaf block, whether it is the sole index of a leaf-form
    /// directory or one leaf of a node-form B-tree.
    ///
    /// Only the magic distinguishes the two, and the difference matters:
    /// a leaf1 block ends with a `bests` array and a tail, a leafn block
    /// does not.
    ///
    /// Big-endian.
    pub mod leaf_hdr {
        /// The block-info prefix, [`super::da_blk`], at offset 0.
        pub const INFO: usize = 0;
        /// `u16`. Number of hash index records, stale ones included.
        /// At 12 on v4 and 56 on v5.
        pub const fn count(is_v5: bool) -> usize {
            if is_v5 {
                super::da_blk::SIZE_V5
            } else {
                super::da_blk::SIZE_V4
            }
        }
        /// `u16`. How many records are stale. Immediately after
        /// `count`, and in the slot a node header uses for its level.
        pub const fn stale(is_v5: bool) -> usize {
            count(is_v5) + 2
        }
        /// `u32`. v5 padding, after the pair. It is the reason the
        /// records start at 64 rather than at 60.
        pub const V5_PAD: usize = super::da_blk::SIZE_V5 + 4;
        /// Size of the v4 header, and so the offset of its first record.
        pub const V4_SIZE: usize = super::super::XFS_DIR2_LEAF_HDR_SIZE;
        /// Size of the v5 header, and so the offset of its first record.
        pub const V5_SIZE: usize = super::super::XFS_DIR3_LEAF_HDR_SIZE;
    }

    /// `xfs_dir2_leaf_entry` — one hash index record, used both in a
    /// leaf block and in the tail of a block-form directory.
    ///
    /// Records are sorted by hash and never by name, so a lookup hashes
    /// the name and binary-searches. Equal hashes are permitted and are
    /// resolved by reading each candidate's name, which means a lookup
    /// must be prepared to try more than one record.
    ///
    /// Big-endian.
    pub mod leaf_entry {
        /// `u32`. Hash of the entry's name. The algorithm is XFS's
        /// `xfs_da_hashname`, which is not represented in this module.
        pub const HASHVAL: usize = 0;
        /// `u32`. Where the entry lives: its byte offset within the
        /// directory's **data space**, divided by 8. Multiply by 8 to
        /// get a byte offset, then divide by the directory block size
        /// for the block and take the remainder for the offset within
        /// it.
        ///
        /// A useful check falls out of this: `.` is always the first
        /// entry of data block 0, so its address is the data header size
        /// divided by 8, which is 2 on v4 and 8 on v5.
        ///
        /// Zero is [`super::super::XFS_DIR2_NULL_DATAPTR`], a stale
        /// record.
        pub const ADDRESS: usize = 4;
        /// Size of one record.
        pub const SIZE: usize = super::super::XFS_DIR2_LEAF_ENTRY_SIZE;
    }

    /// `xfs_dir2_leaf_tail` — the last four bytes of a leaf-form
    /// directory's single index block, at `dirblocksize - 4`.
    ///
    /// Only a leaf1 block has one. A leafn block, one leaf among many in
    /// a node-form directory, has no tail and no `bests`, because no
    /// single leaf can summarise the free space of a directory whose
    /// index no longer fits in one block. That is the whole reason the
    /// free-index space exists.
    ///
    /// Big-endian.
    pub mod leaf_tail {
        /// `u32`. Length of the `bests` array in front of the tail,
        /// which is also the number of data blocks the directory has.
        pub const BESTCOUNT: usize = 0;
        /// Size of the tail.
        pub const SIZE: usize = super::super::XFS_DIR2_LEAF_TAIL_SIZE;
        /// Where the tail starts in a block of the given size.
        pub const fn at(dirblocksize: usize) -> usize {
            dirblocksize - SIZE
        }
        /// Where the `bests` array starts: `bestcount` `u16`s ending
        /// where the tail begins.
        ///
        /// The array's *region* extends downwards as data blocks are
        /// added, but within it index 0 is still at the lowest address
        /// and the array runs upwards towards the tail. Confirmed by
        /// observation: a two-entry array in a 4096-byte block put
        /// `bests[0]` at 0xff8, `bests[1]` at 0xffa and `bestcount` at
        /// 0xffc.
        pub const fn bests_start(dirblocksize: usize, bestcount: usize) -> usize {
            at(dirblocksize) - bestcount * super::super::XFS_DIR2_BEST_SIZE
        }
    }

    /// `xfs_da_node_hdr` (v4) and `xfs_da3_node_hdr` (v5) — the head of
    /// an interior node of the directory's hash B-tree.
    ///
    /// Byte for byte the same shape as a leaf header, with `level` in
    /// the slot the leaf uses for `stale`. Nothing but the magic
    /// distinguishes them, so reading a leaf as a node yields a
    /// plausible level that happens to be the stale count.
    ///
    /// Big-endian.
    pub mod node_hdr {
        /// The block-info prefix, [`super::da_blk`], at offset 0.
        pub const INFO: usize = 0;
        /// `u16`. Number of child records. At 12 on v4, 56 on v5.
        pub const fn count(is_v5: bool) -> usize {
            if is_v5 {
                super::da_blk::SIZE_V5
            } else {
                super::da_blk::SIZE_V4
            }
        }
        /// `u16`. Height above the leaves, which are level 0, so this is
        /// at least 1 and at most
        /// [`super::super::XFS_DA_NODE_MAXDEPTH`]. This is the field a
        /// leaf keeps its stale count in.
        pub const fn level(is_v5: bool) -> usize {
            count(is_v5) + 2
        }
        /// `u32`. v5 padding after the pair.
        pub const V5_PAD: usize = super::da_blk::SIZE_V5 + 4;
        /// Size of the v4 header, and so the offset of its first record.
        pub const V4_SIZE: usize = super::super::XFS_DA_NODE_HDR_SIZE;
        /// Size of the v5 header, and so the offset of its first record.
        pub const V5_SIZE: usize = super::super::XFS_DA3_NODE_HDR_SIZE;
    }

    /// `xfs_da_node_entry` — one child record of an interior node.
    ///
    /// Big-endian.
    pub mod node_entry {
        /// `u32`. The highest hash value anywhere in the subtree below
        /// `BEFORE`. A lookup takes the first record whose bound is at
        /// or above the hash it wants; a hash above every bound in the
        /// root is simply not in the directory.
        pub const HASHVAL: usize = 0;
        /// `u32`. The child block, as an `xfs_dablk_t`, which counts
        /// **filesystem** blocks within the directory's address space
        /// and not directory blocks. On a filesystem with 16 KiB
        /// directory blocks over 4 KiB filesystem blocks these values
        /// step by four.
        pub const BEFORE: usize = 4;
        /// Size of one record.
        pub const SIZE: usize = super::super::XFS_DA_NODE_ENTRY_SIZE;
    }

    /// `xfs_dir2_free_hdr` (v4) and `xfs_dir3_free_hdr` (v5) — the head
    /// of a free-index block, which node-form and larger directories
    /// have and smaller ones do not.
    ///
    /// A free-index block is nothing but a header and a `bests` array:
    /// one `u16` per data block, holding that block's
    /// `bestfree[0].length`. It is what a leaf-form directory keeps in
    /// its leaf tail, moved out into blocks of its own once there is
    /// more than one leaf.
    ///
    /// The v5 layout is not in the published specification, which
    /// predates it. It was established by observation: a v5 free-index
    /// block reported `firstdb` 0, `nvalid` 24, `nused` 24 and
    /// `bests[23]` of 0x2c0, and the bytes put those at 48, 52, 56 and
    /// 0x6e respectively.
    ///
    /// Big-endian.
    pub mod free_hdr {
        /// `u32`. v4 only, at offset 0. On v5 the magic is
        /// [`super::dir3_blk::MAGIC`], also at offset 0.
        pub const V4_MAGIC: usize = 0;
        /// `i32`. Directory-block number of the data block that
        /// `bests[0]` describes.
        ///
        /// An `xfs_dir2_db_t`, so it counts **directory** blocks, unlike
        /// the `xfs_dablk_t` in a node record. It is non-zero only when
        /// a directory needs more than one free-index block, which takes
        /// hundreds of thousands of entries.
        pub const fn firstdb(is_v5: bool) -> usize {
            if is_v5 {
                super::dir3_blk::SIZE
            } else {
                4
            }
        }
        /// `i32`. Number of entries in the `bests` array, which is the
        /// index of the last data block plus one.
        pub const fn nvalid(is_v5: bool) -> usize {
            firstdb(is_v5) + 4
        }
        /// `i32`. How many of those entries describe a data block that
        /// is actually present.
        ///
        /// `nused <= nvalid`, and the difference is the number of
        /// entries holding
        /// [`super::super::XFS_DIR2_DATA_FREE_NULL`] for a data block
        /// that was freed and left a hole.
        ///
        /// The published specification contradicts itself here: its
        /// diagram labels `nvalid` the number of elements in the array
        /// and `nused` the number of valid ones, which gives the
        /// ordering above, while its prose says `nvalid` "will always be
        /// less than or equal to `nused`". Every worked example has the
        /// two equal, so they settle nothing. Observation does: a
        /// directory with four data blocks emptied reported `nvalid` 24
        /// against `nused` 20, with four `0xffff` entries in the array.
        /// The diagram is right and the prose is wrong.
        pub const fn nused(is_v5: bool) -> usize {
            firstdb(is_v5) + 8
        }
        /// `u32`. v5 padding after the three counts.
        pub const V5_PAD: usize = super::dir3_blk::SIZE + 12;
        /// Size of the v4 header, and so the offset of `bests[0]`.
        pub const V4_SIZE: usize = super::super::XFS_DIR2_FREE_HDR_SIZE;
        /// Size of the v5 header, and so the offset of `bests[0]`.
        pub const V5_SIZE: usize = super::super::XFS_DIR3_FREE_HDR_SIZE;
        /// Offset of `bests[i]`.
        ///
        /// The array starts directly after the header and runs
        /// **upwards**, which is the opposite of the leaf-form
        /// directory's array and the opposite of what the published
        /// specification's prose says about this structure. Its own hex
        /// example contradicts its prose, and observation of a v5 block
        /// agrees with the example.
        pub const fn bests(is_v5: bool, i: usize) -> usize {
            let start = if is_v5 { V5_SIZE } else { V4_SIZE };
            start + i * super::super::XFS_DIR2_BEST_SIZE
        }
        /// How many `bests` entries fit in one free-index block, and so
        /// how many data blocks one block can describe: 2040 on a 4 KiB
        /// v4 filesystem, which is the number the specification's
        /// B+tree-directory example shows, and 2016 on a v5 one, where
        /// the larger header costs 24 entries.
        pub const fn max_bests(dirblocksize: usize, is_v5: bool) -> usize {
            let start = if is_v5 { V5_SIZE } else { V4_SIZE };
            (dirblocksize - start) / super::super::XFS_DIR2_BEST_SIZE
        }
    }

    /// Offset of the two-`u16` pair that follows the block-info header
    /// in a leaf or a node block.
    ///
    /// The two structures put different fields in the same two slots: a
    /// leaf's second field is its stale count, a node's is its level.
    ///
    /// v5 headers end with four bytes of padding after the pair and v4
    /// headers have none, so the pair sits eight bytes before the end of
    /// a v5 header and four before the end of a v4 one. Deriving it from
    /// the header size rather than from the block-info size keeps the
    /// padding accounted for in one place.
    pub const fn da_counts(hdr_size: usize, is_v5: bool) -> usize {
        hdr_size - if is_v5 { 8 } else { 4 }
    }
}

// ---------------------------------------------------------------------
// What is deliberately not here
// ---------------------------------------------------------------------
//
// `xfs_da_hashname`, the function that turns a name into the hash the
// index is sorted by. It is an algorithm rather than a layout, this
// crate does not implement it, and it is not described in the published
// specification, which names the kernel source file instead. A lookup
// that cannot hash can still read a directory: walk the data blocks and
// compare names. The index is an optimisation, and everything needed to
// verify it is consistent — sorted order, stale counts, addresses that
// land on real entries — is representable without it.
//
// Directory offset cookies beyond the raw `offset` field: how a `readdir`
// position is composed from a block number and an offset is a matter of
// interface rather than of layout, and the parser returns the stored
// value without interpreting it.
