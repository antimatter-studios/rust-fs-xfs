//! The free-space B+trees: which blocks of an allocation group are free.
//!
//! Every allocation group keeps the same free space in **two** trees.
//! `bnobt` is keyed by starting block, so a freed extent can be found
//! next to its neighbours and merged with them. `cntbt` is keyed by
//! length, so a request for *n* blocks can find the best fit without
//! scanning. The two hold exactly the same extents in different orders,
//! and an allocator that updates one without the other leaves the group
//! describing two different filesystems.
//!
//! Reading them is what truncate and allocation need before they can
//! write anything: freeing an extent means inserting it into both trees,
//! and inserting it means knowing what is already there.
//!
//! # The short form
//!
//! These are *short-form* B+trees — pointers are 32-bit block numbers
//! relative to the allocation group, rather than the 64-bit filesystem
//! blocks the block-map tree uses. That makes the header a different
//! size from [`crate::bmbt`]'s, which is the one thing about them most
//! likely to be got wrong by analogy.
//!
//! ```text
//! v5 header, 56 bytes          v4 header, 16 bytes
//!  0 u32 bb_magic               0 u32 bb_magic
//!  4 u16 bb_level               4 u16 bb_level
//!  6 u16 bb_numrecs             6 u16 bb_numrecs
//!  8 u32 bb_leftsib             8 u32 bb_leftsib
//! 12 u32 bb_rightsib           12 u32 bb_rightsib
//! 16 u64 bb_blkno   (basic blocks, this block's own address)
//! 24 u64 bb_lsn
//! 32 uuid bb_uuid   (16 bytes)
//! 48 u32 bb_owner   (the allocation group)
//! 52 u32 bb_crc
//! ```
//!
//! A record is eight bytes — a start block and a length, both 32-bit
//! and both relative to the group. Keys are the same eight bytes; a
//! child pointer is four.
//!
//! Both sizes were read off filesystems rather than assumed: a v5
//! `bnobt` root has its first record at offset 56 and a v4 one at
//! offset 16, and in both cases the records sum to the `agf_freeblks`
//! recorded in the allocation-group header beside them.
//!
//! # Why the pointer array does not start after the keys
//!
//! In an internal node the keys occupy room for the **maximum** number
//! of records the block could hold, not the number it does hold, and
//! the pointers begin after that reserved span. Reading them from after
//! the keys in use lands in the middle of the key array on any node
//! that is not full, which is most of them.

use crate::ag::Agf;
use crate::endian::{be16, be32, be64, le32, uuid_at};
use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// `ABTB` — free space by block, v4.
pub const XFS_ABTB_MAGIC: u32 = 0x4142_5442;
/// `AB3B` — free space by block, v5.
pub const XFS_ABTB_CRC_MAGIC: u32 = 0x4142_3342;
/// `ABTC` — free space by count, v4.
pub const XFS_ABTC_MAGIC: u32 = 0x4142_5443;
/// `AB3C` — free space by count, v5.
pub const XFS_ABTC_CRC_MAGIC: u32 = 0x4142_3343;

/// The v4 short-form header: magic, level, record count and two
/// siblings.
const V4_HEADER_LEN: usize = 16;

/// The v5 short-form header, which adds the block's own address, a
/// sequence number, the filesystem UUID, the owning group and a
/// checksum.
const V5_HEADER_LEN: usize = 56;

/// A record: a start block and a length, both relative to the group.
const RECORD_LEN: usize = 8;
/// A key is the same shape as the record it indexes.
const KEY_LEN: usize = 8;
/// A child pointer is an allocation-group block number.
const PTR_LEN: usize = 4;

/// A tree deeper than this is not a tree, and the bound stops a cycle
/// in a corrupt image from being walked forever.
const MAX_LEVELS: u16 = 9;

/// Byte offsets within the short-form block header.
mod offsets {
    pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;
    pub const BLKNO: usize = 16;
    pub const UUID: usize = 32;
    pub const OWNER: usize = 48;
    pub const CRC: usize = 52;
}

/// Which of the two trees, which decides both the magic to expect and
/// the order the records come back in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// `bnobt`, keyed by starting block.
    ByBlock,
    /// `cntbt`, keyed by length, then by starting block.
    ByCount,
}

impl Order {
    /// The magic a block of this tree carries.
    fn magic(self, v5: bool) -> u32 {
        match (self, v5) {
            (Order::ByBlock, true) => XFS_ABTB_CRC_MAGIC,
            (Order::ByBlock, false) => XFS_ABTB_MAGIC,
            (Order::ByCount, true) => XFS_ABTC_CRC_MAGIC,
            (Order::ByCount, false) => XFS_ABTC_MAGIC,
        }
    }

    /// The name to use when reporting a problem.
    fn name(self) -> &'static str {
        match self {
            Order::ByBlock => "bnobt",
            Order::ByCount => "cntbt",
        }
    }
}

/// A run of free blocks within one allocation group.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FreeExtent {
    /// First free block, relative to the allocation group.
    pub startblock: u32,
    /// How many blocks the run covers.
    pub blockcount: u32,
}

impl FreeExtent {
    /// One past the last block of the run.
    ///
    /// Widened to 64 bits deliberately: a run ending at the last block
    /// of a full group overflows a `u32`, and a bounds check written
    /// against the wrapped value passes on exactly the extent that
    /// would corrupt the group.
    pub fn end(&self) -> u64 {
        u64::from(self.startblock) + u64::from(self.blockcount)
    }
}

/// A header that has been read and checked.
struct Node {
    level: u16,
    numrecs: u16,
    /// Where the records or keys begin.
    body: usize,
    /// How many records the block could hold.
    maxrecs: usize,
}

/// How many records of `len` bytes fit in a block's body.
fn maxrecs(space: usize, len: usize) -> usize {
    space / len
}

/// Read and check one block of the tree.
///
/// `expect_level` is the level the parent said this child sits at and
/// `agno` the group the tree belongs to. Checking both is what makes
/// the descent self-verifying: a block that belongs to another group,
/// or sits at a different depth than its parent believed, is rejected
/// before its contents are read as free space — which matters more here
/// than in a file's tree, since the consequence of believing a stale
/// block is handing out space that is in use.
fn parse_block(
    buf: &[u8],
    sb: &Superblock,
    order: Order,
    agno: u32,
    agblock: u32,
    expect_level: u16,
) -> Result<Node> {
    let header = if sb.is_v5() {
        V5_HEADER_LEN
    } else {
        V4_HEADER_LEN
    };
    let what = order.name();
    if buf.len() < header {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is {} bytes, shorter than its {header}-byte header",
            buf.len()
        )));
    }

    let want = order.magic(sb.is_v5());
    let magic = be32(buf, offsets::MAGIC);
    if magic != want {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} has magic {magic:#010x}, expected {want:#010x}"
        )));
    }

    if sb.is_v5() {
        let stored = le32(buf, offsets::CRC);
        if stored != crc32c_with_zeroed_crc(buf, offsets::CRC) {
            return Err(Error::ChecksumMismatch {
                what: "free-space btree block",
                block: u64::from(agblock),
            });
        }
        if uuid_at(buf, offsets::UUID) != sb.meta_uuid {
            return Err(Error::BlockIdentityMismatch {
                what: "free-space btree block",
                expected: u64::from(agblock),
                found: u64::MAX, // a UUID mismatch says nothing about the address
            });
        }
        // The owner is the group. A block from a different group would
        // otherwise decode into entirely plausible extents belonging to
        // somewhere else.
        let owner = be32(buf, offsets::OWNER);
        if owner != agno {
            return Err(Error::BlockIdentityMismatch {
                what: "free-space btree block owner",
                expected: u64::from(agno),
                found: u64::from(owner),
            });
        }
        // The block records its own address, so a block read from the
        // wrong place says so rather than being believed.
        let stated = be64(buf, offsets::BLKNO);
        let expected = expected_blkno(sb, agno, agblock);
        if stated != expected {
            return Err(Error::BlockIdentityMismatch {
                what: "free-space btree block address",
                expected,
                found: stated,
            });
        }
    }

    let level = be16(buf, offsets::LEVEL);
    if level != expect_level {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is at level {level}, but its parent points to it \
             as level {expect_level}"
        )));
    }

    let space = buf.len() - header;
    let per = if level == 0 {
        RECORD_LEN
    } else {
        KEY_LEN + PTR_LEN
    };
    let max = maxrecs(space, per);
    let numrecs = be16(buf, offsets::NUMRECS);
    if usize::from(numrecs) > max {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} claims {numrecs} records but has room for {max}"
        )));
    }

    Ok(Node {
        level,
        numrecs,
        body: header,
        maxrecs: max,
    })
}

/// Where a group's block sits on the device, in 512-byte basic blocks.
///
/// The same unit `bb_blkno` is written in, and the same unit the buffer
/// log item addresses blocks by — which is why this is shared rather
/// than recomputed at each use.
pub fn expected_blkno(sb: &Superblock, agno: u32, agblock: u32) -> u64 {
    let fsblock = u64::from(agno) * u64::from(sb.agblocks) + u64::from(agblock);
    fsblock * u64::from(sb.blocksize) / crate::log::BBSIZE as u64
}

/// Collect every free extent in one of a group's two trees, in the
/// order the tree keeps them.
///
/// `read_agblock` fetches one block of the group by its group-relative
/// number; the walker never assumes a block is where a cache would put
/// it.
///
/// # Errors
///
/// [`Error::BadSuperblock`] for a malformed block or an impossible
/// depth, [`Error::ChecksumMismatch`] and [`Error::BlockIdentityMismatch`]
/// for a block that is not the one that was asked for, and whatever
/// `read_agblock` returns.
pub fn walk<F>(
    sb: &Superblock,
    order: Order,
    agno: u32,
    root: u32,
    levels: u32,
    mut read_agblock: F,
) -> Result<Vec<FreeExtent>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    if levels == 0 || levels > u32::from(MAX_LEVELS) {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {} claims {levels} levels, which is not a tree",
            order.name()
        )));
    }

    let mut out = Vec::new();
    // Depth-first, left to right, so records arrive in the tree's own
    // order and a caller can check that ordering rather than impose it.
    let mut stack = vec![(root, (levels - 1) as u16)];

    while let Some((agblock, expect_level)) = stack.pop() {
        let buf = read_agblock(agblock)?;
        let node = parse_block(&buf, sb, order, agno, agblock, expect_level)?;

        if node.level == 0 {
            let end = node.body + usize::from(node.numrecs) * RECORD_LEN;
            if end > buf.len() {
                return Err(Error::BadSuperblock(format!(
                    "AG {agno}: {} leaf {agblock} needs {end} bytes for its {} records \
                     but is only {} long",
                    order.name(),
                    node.numrecs,
                    buf.len()
                )));
            }
            for i in 0..usize::from(node.numrecs) {
                let at = node.body + i * RECORD_LEN;
                out.push(FreeExtent {
                    startblock: be32(&buf, at),
                    blockcount: be32(&buf, at + 4),
                });
            }
            continue;
        }

        // The pointers start after room for the maximum number of keys,
        // not after the keys in use.
        let first = node.body + node.maxrecs * KEY_LEN;
        let end = first + usize::from(node.numrecs) * PTR_LEN;
        if end > buf.len() {
            return Err(Error::BadSuperblock(format!(
                "AG {agno}: {} node {agblock} needs {end} bytes for its pointer array \
                 but is only {} long",
                order.name(),
                buf.len()
            )));
        }
        // Pushed in reverse so the leftmost child is visited first.
        for i in (0..usize::from(node.numrecs)).rev() {
            stack.push((be32(&buf, first + i * PTR_LEN), node.level - 1));
        }
    }

    Ok(out)
}

/// Walk the tree an allocation-group header points at.
///
/// The root and depth come from the header rather than from the caller,
/// which is the only place they are authoritative.
///
/// # Errors
///
/// As [`walk`].
pub fn walk_from_agf<F>(
    sb: &Superblock,
    agf: &Agf,
    order: Order,
    read_agblock: F,
) -> Result<Vec<FreeExtent>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    let which = match order {
        Order::ByBlock => crate::ag::agf_btree::BNO,
        Order::ByCount => crate::ag::agf_btree::CNT,
    };
    walk(
        sb,
        order,
        agf.seqno,
        agf.roots[which],
        agf.levels[which],
        read_agblock,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run ending at the last block of a full group must not wrap.
    ///
    /// A `u32` end computed for an extent at the very top of the address
    /// space comes out as zero, and a bounds check written against it
    /// passes on exactly the extent that would be out of range.
    #[test]
    fn the_end_of_an_extent_does_not_wrap() {
        let extent = FreeExtent {
            startblock: u32::MAX - 4,
            blockcount: 8,
        };
        assert_eq!(extent.end(), u64::from(u32::MAX) + 4);
        assert!(extent.end() > u64::from(extent.startblock));
    }

    /// Each tree has its own magic in each format version, and nothing
    /// else does.
    #[test]
    fn the_two_trees_are_told_apart_by_magic() {
        let all = [
            Order::ByBlock.magic(true),
            Order::ByBlock.magic(false),
            Order::ByCount.magic(true),
            Order::ByCount.magic(false),
        ];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b, "two magics collide");
            }
        }
        // Readable as ASCII on the wire, which is how they are found in
        // a hex dump.
        assert_eq!(&Order::ByBlock.magic(true).to_be_bytes(), b"AB3B");
        assert_eq!(&Order::ByCount.magic(true).to_be_bytes(), b"AB3C");
        assert_eq!(&Order::ByBlock.magic(false).to_be_bytes(), b"ABTB");
        assert_eq!(&Order::ByCount.magic(false).to_be_bytes(), b"ABTC");
    }

    /// The two header sizes, which are the thing most likely to be got
    /// wrong by analogy with the block-map tree's 72-byte v5 header.
    #[test]
    fn the_short_form_headers_are_smaller_than_the_long_form() {
        assert_eq!(V4_HEADER_LEN, 16);
        assert_eq!(V5_HEADER_LEN, 56);
        // Where the v5 header's fields land, as read off a real root.
        assert_eq!(offsets::BLKNO + 8, 24);
        assert_eq!(offsets::UUID + 16, offsets::OWNER);
        assert_eq!(offsets::OWNER + 4, offsets::CRC);
        assert_eq!(offsets::CRC + 4, V5_HEADER_LEN);
    }

    /// A block holds as many records as fit, and an internal node holds
    /// fewer because each entry carries a pointer as well as a key.
    #[test]
    fn an_internal_node_holds_fewer_entries_than_a_leaf() {
        let space = 4096 - V5_HEADER_LEN;
        assert_eq!(maxrecs(space, RECORD_LEN), 505);
        assert_eq!(maxrecs(space, KEY_LEN + PTR_LEN), 336);
    }
}
