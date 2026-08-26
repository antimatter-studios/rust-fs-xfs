//! The inode B+trees: which inodes an allocation group holds, and which
//! of them are free.
//!
//! Inodes are allocated in **chunks** rather than one at a time, and a
//! group's `inobt` records where each chunk starts and which of its
//! inodes are in use. Creating a file means finding a free inode in a
//! chunk, or allocating a whole new chunk when none has one; removing a
//! file means giving one back. Nothing can create or unlink until these
//! can be read.
//!
//! A v5 filesystem also keeps a `finobt` holding **only** the chunks
//! that have a free inode left, so a create does not have to walk the
//! whole `inobt` to find one. The two must agree, which is what makes
//! them checkable against each other.
//!
//! # The record, and the thing that is not obvious about it
//!
//! Sixteen bytes, in every version. What is in the middle four depends
//! not on the format version but on a **feature bit**:
//!
//! ```text
//! v4, and v5 without sparse inodes
//!   0 u32 ir_startino
//!   4 u32 ir_freecount
//!   8 u64 ir_free        one bit per inode, set when free
//!
//! v5 with sparse inodes
//!   0 u32 ir_startino
//!   4 u16 ir_holemask    one bit per four inodes, set when absent
//!   6 u8  ir_count       inodes the chunk actually holds
//!   7 u8  ir_freecount
//!   8 u64 ir_free
//! ```
//!
//! That it follows the feature and not the version was not assumed. A v5
//! filesystem made with `-i sparse=0` writes `0000003d` where one made
//! with sparse inodes writes `0000403d` for the same 61 free inodes of
//! the same 64-inode chunk — the first a plain count, the second a hole
//! mask of zero, a count of 64 and a free count of 61 packed together.
//!
//! Reading the packed form on a filesystem that wrote the plain one
//! gives a chunk of 0 inodes with 61 of them free, which is not merely
//! wrong but self-contradictory — and the other way round gives a chunk
//! of 16,445 free inodes, which is refused here for the same reason.
//!
//! # What the fixtures cannot tell apart
//!
//! Worth stating, because it looked at first as though they could.
//!
//! For a chunk that is **full** — no holes, all 64 inodes present — the
//! two readings agree on everything that matters. The hole mask is zero
//! and the count is 64, which is what the plain branch assumes anyway,
//! and the free count survives being read as the low byte of a 32-bit
//! field. So a reader that ignored the feature bit and always took the
//! plain branch passes every check in `inode_btree_oracle`, because no
//! fixture holds a chunk that is genuinely sparse.
//!
//! Only a chunk with a hole in it distinguishes them, and producing one
//! means fragmenting a group hard enough that a whole chunk of inodes
//! cannot be allocated contiguously. Until a fixture does that, the
//! packed branch is exercised on real filesystems but the *distinction*
//! is not, and the guard above is what stands in for it: a plain read of
//! a packed record is refused rather than quietly truncated back into a
//! plausible number.

use crate::ag::Agi;
use crate::endian::{be16, be32, be64, le32, uuid_at};
use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// `IABT` — inodes, v4.
pub const XFS_IBT_MAGIC: u32 = 0x4941_4254;
/// `IAB3` — inodes, v5.
pub const XFS_IBT_CRC_MAGIC: u32 = 0x4941_4233;
/// `FIBT` — free inodes, v4. The free-inode tree is a v5 feature, so
/// this exists for completeness rather than because it has been seen.
pub const XFS_FIBT_MAGIC: u32 = 0x4649_4254;
/// `FIB3` — free inodes, v5.
pub const XFS_FIBT_CRC_MAGIC: u32 = 0x4649_4233;

/// The v4 short-form header.
const V4_HEADER_LEN: usize = 16;
/// The v5 short-form header, which adds the block's address, a sequence
/// number, the UUID, the owning group and a checksum.
const V5_HEADER_LEN: usize = 56;

/// A record, in every version.
const RECORD_LEN: usize = 16;
/// A key is the starting inode alone.
const KEY_LEN: usize = 4;
/// A child pointer is an allocation-group block number.
const PTR_LEN: usize = 4;

/// Inodes in a full chunk, and the width of the free bitmap.
pub const INODES_PER_CHUNK: u8 = 64;

/// Inodes one hole-mask bit covers.
///
/// A chunk is sparse in units of four inodes rather than one, which is
/// why the mask is sixteen bits wide and not sixty-four.
pub const INODES_PER_HOLEMASK_BIT: u8 = 4;

/// A tree deeper than this is not a tree.
const MAX_LEVELS: u16 = 9;

mod offsets {
    pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;
    pub const BLKNO: usize = 16;
    pub const UUID: usize = 32;
    pub const OWNER: usize = 48;
    pub const CRC: usize = 52;
}

/// Which of a group's two inode trees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    /// `inobt` — every chunk the group holds.
    All,
    /// `finobt` — only the chunks with a free inode left.
    WithFreeInodes,
}

impl Which {
    fn magic(self, v5: bool) -> u32 {
        match (self, v5) {
            (Which::All, true) => XFS_IBT_CRC_MAGIC,
            (Which::All, false) => XFS_IBT_MAGIC,
            (Which::WithFreeInodes, true) => XFS_FIBT_CRC_MAGIC,
            (Which::WithFreeInodes, false) => XFS_FIBT_MAGIC,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Which::All => "inobt",
            Which::WithFreeInodes => "finobt",
        }
    }
}

/// One chunk of inodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct InodeChunk {
    /// The chunk's first inode, relative to the allocation group.
    pub startino: u32,
    /// One bit per four inodes, set where the chunk has no blocks
    /// backing them. Always zero without sparse inodes.
    pub holemask: u16,
    /// How many inodes the chunk actually holds — 64 unless it is
    /// sparse.
    pub count: u8,
    /// How many of them are free.
    pub freecount: u8,
    /// One bit per inode, set when that inode is free.
    pub free: u64,
}

impl InodeChunk {
    /// Whether inode `n` of this chunk is free.
    ///
    /// # Panics
    ///
    /// If `n` is not below [`INODES_PER_CHUNK`].
    pub fn is_free(&self, n: u8) -> bool {
        assert!(
            n < INODES_PER_CHUNK,
            "a chunk holds {INODES_PER_CHUNK} inodes"
        );
        self.free & (1u64 << n) != 0
    }

    /// Whether inode `n` of this chunk exists at all.
    ///
    /// A sparse chunk has runs of four inodes with no blocks behind
    /// them. They read as free in [`InodeChunk::free`] and must not be
    /// handed out, which is the whole reason the mask is separate.
    pub fn exists(&self, n: u8) -> bool {
        assert!(
            n < INODES_PER_CHUNK,
            "a chunk holds {INODES_PER_CHUNK} inodes"
        );
        self.holemask & (1u16 << (n / INODES_PER_HOLEMASK_BIT)) == 0
    }

    /// The first inode that is both present and free, if any.
    pub fn first_free(&self) -> Option<u8> {
        (0..INODES_PER_CHUNK).find(|&n| self.exists(n) && self.is_free(n))
    }
}

/// Decode one record.
///
/// `sparse` selects between the two shapes of the middle four bytes; see
/// the module documentation for why it is the feature and not the
/// version that decides.
///
/// # Errors
///
/// [`Error::BadSuperblock`] if a plain free count is larger than a chunk
/// can hold. That is the shape a packed record takes when it is read
/// plainly — 61 free inodes of a 64-inode chunk pack to `0x0000403d`,
/// which reads plainly as 16,445 — so refusing it turns the one
/// misreading the arithmetic below would not otherwise catch into an
/// error rather than a silent truncation back to a plausible number.
fn record(buf: &[u8], at: usize, sparse: bool) -> Result<InodeChunk> {
    let startino = be32(buf, at);
    let free = be64(buf, at + 8);
    if sparse {
        return Ok(InodeChunk {
            startino,
            holemask: be16(buf, at + 4),
            count: buf[at + 6],
            freecount: buf[at + 7],
            free,
        });
    }

    let freecount = be32(buf, at + 4);
    if freecount > u32::from(INODES_PER_CHUNK) {
        return Err(Error::BadSuperblock(format!(
            "an inode chunk at {startino} claims {freecount} free inodes, more than the              {INODES_PER_CHUNK} a chunk holds — which is what a packed record looks like              when it is read as a plain one"
        )));
    }
    Ok(InodeChunk {
        startino,
        holemask: 0,
        count: INODES_PER_CHUNK,
        freecount: freecount as u8,
        free,
    })
}

/// A header that has been read and checked.
struct Node {
    level: u16,
    numrecs: u16,
    body: usize,
    maxrecs: usize,
}

fn parse_block(
    buf: &[u8],
    sb: &Superblock,
    which: Which,
    agno: u32,
    agblock: u32,
    expect_level: u16,
) -> Result<Node> {
    let header = if sb.is_v5() {
        V5_HEADER_LEN
    } else {
        V4_HEADER_LEN
    };
    let what = which.name();
    if buf.len() < header {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is {} bytes, shorter than its {header}-byte header",
            buf.len()
        )));
    }

    let want = which.magic(sb.is_v5());
    let magic = be32(buf, offsets::MAGIC);
    if magic != want {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} has magic {magic:#010x}, expected {want:#010x}"
        )));
    }

    if sb.is_v5() {
        if le32(buf, offsets::CRC) != crc32c_with_zeroed_crc(buf, offsets::CRC) {
            return Err(Error::ChecksumMismatch {
                what: "inode btree block",
                block: u64::from(agblock),
            });
        }
        if uuid_at(buf, offsets::UUID) != sb.meta_uuid {
            return Err(Error::BlockIdentityMismatch {
                what: "inode btree block",
                expected: u64::from(agblock),
                found: u64::MAX,
            });
        }
        let owner = be32(buf, offsets::OWNER);
        if owner != agno {
            return Err(Error::BlockIdentityMismatch {
                what: "inode btree block owner",
                expected: u64::from(agno),
                found: u64::from(owner),
            });
        }
        let stated = be64(buf, offsets::BLKNO);
        let expected = crate::alloc_btree::expected_blkno(sb, agno, agblock);
        if stated != expected {
            return Err(Error::BlockIdentityMismatch {
                what: "inode btree block address",
                expected,
                found: stated,
            });
        }
    }

    let level = be16(buf, offsets::LEVEL);
    if level != expect_level {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is at level {level}, but its parent points to \
             it as level {expect_level}"
        )));
    }

    let space = buf.len() - header;
    let per = if level == 0 {
        RECORD_LEN
    } else {
        KEY_LEN + PTR_LEN
    };
    let max = space / per;
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

/// Collect every chunk in one of a group's inode trees, in the order the
/// tree keeps them — by starting inode.
///
/// # Errors
///
/// As [`crate::alloc_btree::walk`], and whatever `read_agblock` returns.
pub fn walk<F>(
    sb: &Superblock,
    which: Which,
    agno: u32,
    root: u32,
    levels: u32,
    mut read_agblock: F,
) -> Result<Vec<InodeChunk>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    if levels == 0 || levels > u32::from(MAX_LEVELS) {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {} claims {levels} levels, which is not a tree",
            which.name()
        )));
    }
    let sparse = sb.has_sparse_inodes();

    let mut out = Vec::new();
    let mut stack = vec![(root, (levels - 1) as u16)];

    while let Some((agblock, expect_level)) = stack.pop() {
        let buf = read_agblock(agblock)?;
        let node = parse_block(&buf, sb, which, agno, agblock, expect_level)?;

        if node.level == 0 {
            let end = node.body + usize::from(node.numrecs) * RECORD_LEN;
            if end > buf.len() {
                return Err(Error::BadSuperblock(format!(
                    "AG {agno}: {} leaf {agblock} needs {end} bytes for its {} records but is \
                     only {} long",
                    which.name(),
                    node.numrecs,
                    buf.len()
                )));
            }
            for i in 0..usize::from(node.numrecs) {
                out.push(record(&buf, node.body + i * RECORD_LEN, sparse)?);
            }
            continue;
        }

        // The pointers begin after room for the maximum number of keys,
        // not after the keys in use.
        let first = node.body + node.maxrecs * KEY_LEN;
        let end = first + usize::from(node.numrecs) * PTR_LEN;
        if end > buf.len() {
            return Err(Error::BadSuperblock(format!(
                "AG {agno}: {} node {agblock} needs {end} bytes for its pointer array but is \
                 only {} long",
                which.name(),
                buf.len()
            )));
        }
        for i in (0..usize::from(node.numrecs)).rev() {
            stack.push((be32(&buf, first + i * PTR_LEN), node.level - 1));
        }
    }

    Ok(out)
}

/// Walk the tree an allocation-group inode header points at.
///
/// Returns `None` for the free-inode tree on a filesystem that has none,
/// which is every v4 filesystem and any v5 one made without `finobt`.
///
/// # Errors
///
/// As [`walk`].
pub fn walk_from_agi<F>(
    sb: &Superblock,
    agi: &Agi,
    which: Which,
    read_agblock: F,
) -> Result<Option<Vec<InodeChunk>>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    let (root, levels) = match which {
        Which::All => (agi.root, agi.level),
        Which::WithFreeInodes => {
            if !sb.has_finobt() || agi.free_level == 0 {
                return Ok(None);
            }
            (agi.free_root, agi.free_level)
        }
    };
    walk(sb, which, agi.seqno, root, levels, read_agblock).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same 61 free inodes of the same chunk, written both ways.
    ///
    /// These are the bytes two real filesystems hold — one made with
    /// sparse inodes and one with `-i sparse=0` — and reading either with
    /// the other's rule is what this is here to stop.
    #[test]
    fn the_two_record_shapes_are_not_interchangeable() {
        let mut sparse_bytes = [0u8; RECORD_LEN];
        sparse_bytes[0..4].copy_from_slice(&128u32.to_be_bytes());
        sparse_bytes[4..8].copy_from_slice(&0x0000_403du32.to_be_bytes());
        sparse_bytes[8..16].copy_from_slice(&0xffff_ffff_ffff_fff8u64.to_be_bytes());

        let mut plain_bytes = [0u8; RECORD_LEN];
        plain_bytes[0..4].copy_from_slice(&128u32.to_be_bytes());
        plain_bytes[4..8].copy_from_slice(&61u32.to_be_bytes());
        plain_bytes[8..16].copy_from_slice(&0xffff_ffff_ffff_fff8u64.to_be_bytes());

        let sparse = record(&sparse_bytes, 0, true).expect("a packed record");
        assert_eq!(sparse.count, 64);
        assert_eq!(sparse.freecount, 61);
        assert_eq!(sparse.holemask, 0);

        let plain = record(&plain_bytes, 0, false).expect("a plain record");
        assert_eq!(plain.count, 64);
        assert_eq!(plain.freecount, 61);
        assert_eq!(plain.holemask, 0);

        // Both describe the same chunk, which is the point: for a full
        // chunk the two encodings carry the same facts.
        assert_eq!(sparse, plain, "the same chunk, however it was written");

        // Read with the wrong rule they do not. The plain bytes read as
        // packed give a chunk of no inodes at all — a self-contradiction
        // the arithmetic in the oracle catches.
        assert_eq!(
            record(&plain_bytes, 0, true).expect("decodes").count,
            0,
            "a chunk of no inodes, which is the self-contradiction that gives it away"
        );

        // The other direction is the one that would otherwise pass
        // quietly. A packed record read plainly claims 16,445 free
        // inodes, and truncating that to a byte gives 61 back — the
        // right answer for the wrong reason, on every chunk that is not
        // sparse. It is refused instead.
        assert!(
            record(&sparse_bytes, 0, false).is_err(),
            "a packed record read plainly must be refused, not truncated back into a \
             plausible free count"
        );
    }

    /// The free bitmap and the count have to agree, and they do on a
    /// freshly formatted filesystem: three inodes in use, sixty-one free.
    #[test]
    fn the_bitmap_and_the_count_agree() {
        let chunk = InodeChunk {
            startino: 128,
            holemask: 0,
            count: 64,
            freecount: 61,
            free: 0xffff_ffff_ffff_fff8,
        };
        assert_eq!(chunk.free.count_ones(), u32::from(chunk.freecount));
        assert!(!chunk.is_free(0), "the first three are in use");
        assert!(!chunk.is_free(2));
        assert!(chunk.is_free(3));
        assert_eq!(chunk.first_free(), Some(3));
    }

    /// A hole is not a free inode. It reads as free in the bitmap and
    /// must not be handed out, which is why the mask is separate — and
    /// why the mask covers four inodes per bit rather than one.
    #[test]
    fn a_hole_is_not_a_free_inode() {
        let chunk = InodeChunk {
            startino: 128,
            // The first four inodes have no blocks behind them.
            holemask: 0b1,
            count: 60,
            freecount: 60,
            free: u64::MAX,
        };
        for n in 0..INODES_PER_HOLEMASK_BIT {
            assert!(chunk.is_free(n), "the bitmap says free");
            assert!(!chunk.exists(n), "but there is no inode there");
        }
        assert!(chunk.exists(4));
        assert_eq!(
            chunk.first_free(),
            Some(4),
            "the first inode that is both present and free"
        );
    }

    /// Each tree has its own magic in each version, and they read as
    /// their names in a hex dump.
    #[test]
    fn the_trees_are_told_apart_by_magic() {
        assert_eq!(&Which::All.magic(true).to_be_bytes(), b"IAB3");
        assert_eq!(&Which::All.magic(false).to_be_bytes(), b"IABT");
        assert_eq!(&Which::WithFreeInodes.magic(true).to_be_bytes(), b"FIB3");
        assert_eq!(&Which::WithFreeInodes.magic(false).to_be_bytes(), b"FIBT");
    }
}
