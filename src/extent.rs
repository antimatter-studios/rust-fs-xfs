//! Data fork extent records.
//!
//! An extent maps a run of consecutive file blocks onto a run of
//! consecutive filesystem blocks. A file's data fork is either a plain
//! array of these records (`Format::Extents`) or a B+tree of them
//! (`Format::Btree`) once it has too many to fit in the inode.
//!
//! # The packed record
//!
//! Each record is 16 bytes holding four fields packed across the bit
//! positions of a 128-bit big-endian value:
//!
//! ```text
//!  bit 127        : flag — 1 means the extent is allocated but unwritten
//!  bits 73..=126  : startoff    — file block this extent begins at    (54 bits)
//!  bits 21..=72   : startblock  — filesystem block it maps to         (52 bits)
//!  bits 0..=20    : blockcount  — length in blocks                    (21 bits)
//! ```
//!
//! `startblock` straddles the boundary between the two 64-bit halves,
//! which is the only genuinely fiddly part of the decoding and the one
//! place a shift is easy to get wrong. The unit tests below pin each
//! field independently, and `tests/extent_oracle.rs` checks the result
//! against what the reference debugger reports for the same inode.
//!
//! # Unwritten extents
//!
//! An extent with the flag set has had blocks allocated but never
//! written. It must read back as zeros rather than as whatever those
//! blocks previously held — skipping that check would leak the previous
//! owner's data, so [`Extent::is_unwritten`] is not optional detail.

use crate::endian::be64;
use crate::error::{Error, Result};

/// Size of one packed extent record in bytes.
pub const EXTENT_RECORD_SIZE: usize = 16;

/// Width of the `startoff` field, in bits.
const STARTOFF_BITS: u32 = 54;
/// Width of the `blockcount` field, in bits.
const BLOCKCOUNT_BITS: u32 = 21;
/// Bits of `startblock` carried in the low half of the record.
const STARTBLOCK_BITS_IN_LOW_HALF: u32 = 43;
/// Bits of `startblock` carried in the high half of the record.
const STARTBLOCK_BITS_IN_HIGH_HALF: u32 = 9;

/// The largest value each field can hold, used both to mask on decode
/// and to reject a value too large to encode.
const STARTOFF_MAX: u64 = (1 << STARTOFF_BITS) - 1;
const BLOCKCOUNT_MAX: u64 = (1 << BLOCKCOUNT_BITS) - 1;
const STARTBLOCK_MAX: u64 = (1 << (STARTBLOCK_BITS_IN_LOW_HALF + STARTBLOCK_BITS_IN_HIGH_HALF)) - 1;

/// One extent: a contiguous run of file blocks mapped to a contiguous
/// run of filesystem blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// File block number this extent starts at.
    pub startoff: u64,
    /// Filesystem block number it maps to.
    pub startblock: u64,
    /// Length of the extent in blocks.
    pub blockcount: u64,
    /// Whether the extent is allocated but has never been written.
    /// Such an extent must read back as zeros.
    pub unwritten: bool,
}

impl Extent {
    /// Decode one packed extent record.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] if the buffer is too short or the record
    /// describes a zero-length extent, which no valid fork contains.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < EXTENT_RECORD_SIZE {
            return Err(Error::BadSuperblock(format!(
                "extent record needs {EXTENT_RECORD_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        let high = be64(buf, 0);
        let low = be64(buf, 8);

        let unwritten = high >> 63 != 0;
        let startoff = (high >> STARTBLOCK_BITS_IN_HIGH_HALF) & STARTOFF_MAX;
        // The only field that spans both halves: its top 9 bits are the
        // low 9 bits of `high`, its bottom 43 bits the top 43 of `low`.
        let startblock = ((high & ((1 << STARTBLOCK_BITS_IN_HIGH_HALF) - 1))
            << STARTBLOCK_BITS_IN_LOW_HALF)
            | (low >> BLOCKCOUNT_BITS);
        let blockcount = low & BLOCKCOUNT_MAX;

        if blockcount == 0 {
            return Err(Error::BadSuperblock(
                "extent record has a block count of zero".into(),
            ));
        }
        Ok(Extent {
            startoff,
            startblock,
            blockcount,
            unwritten,
        })
    }

    /// Encode this extent back into its packed form.
    ///
    /// Present so the decoder can be checked by round-trip, and because
    /// the write path will need it. Every `parse` in this crate has a
    /// matching serializer for that reason.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] if any field exceeds the width the format
    /// gives it.
    pub fn to_bytes(&self) -> Result<[u8; EXTENT_RECORD_SIZE]> {
        if self.startoff > STARTOFF_MAX {
            return Err(Error::BadSuperblock(format!(
                "startoff {} exceeds the {STARTOFF_BITS}-bit field",
                self.startoff
            )));
        }
        if self.startblock > STARTBLOCK_MAX {
            return Err(Error::BadSuperblock(format!(
                "startblock {} exceeds its field width",
                self.startblock
            )));
        }
        if self.blockcount == 0 || self.blockcount > BLOCKCOUNT_MAX {
            return Err(Error::BadSuperblock(format!(
                "blockcount {} is zero or exceeds the {BLOCKCOUNT_BITS}-bit field",
                self.blockcount
            )));
        }
        let high = (u64::from(self.unwritten) << 63)
            | (self.startoff << STARTBLOCK_BITS_IN_HIGH_HALF)
            | (self.startblock >> STARTBLOCK_BITS_IN_LOW_HALF);
        let low = (self.startblock << BLOCKCOUNT_BITS) | self.blockcount;

        let mut out = [0u8; EXTENT_RECORD_SIZE];
        out[0..8].copy_from_slice(&high.to_be_bytes());
        out[8..16].copy_from_slice(&low.to_be_bytes());
        Ok(out)
    }

    /// Whether this extent is allocated but never written, and so must
    /// read back as zeros.
    pub fn is_unwritten(&self) -> bool {
        self.unwritten
    }

    /// One past the last file block this extent covers.
    pub fn end_offset(&self) -> u64 {
        self.startoff + self.blockcount
    }

    /// Whether this extent covers file block `off`.
    pub fn contains(&self, off: u64) -> bool {
        off >= self.startoff && off < self.end_offset()
    }

    /// Map file block `off` to its filesystem block, or `None` when this
    /// extent does not cover it.
    pub fn map(&self, off: u64) -> Option<u64> {
        self.contains(off)
            .then(|| self.startblock + (off - self.startoff))
    }
}

/// Decode a whole extent list from a fork.
///
/// `count` comes from the inode's extent counter rather than from the
/// buffer length, because a fork's tail may be padding.
///
/// # Errors
///
/// Propagates any record-level failure, and rejects a list whose extents
/// are not in ascending file-offset order or which overlap. Both are
/// invariants the format guarantees; a violation means the fork was
/// misread, not that the filesystem is unusual.
pub fn parse_list(buf: &[u8], count: u64) -> Result<Vec<Extent>> {
    let needed = (count as usize)
        .checked_mul(EXTENT_RECORD_SIZE)
        .ok_or_else(|| Error::BadSuperblock("extent count overflows".into()))?;
    if buf.len() < needed {
        return Err(Error::BadSuperblock(format!(
            "fork holds {} bytes, too few for {count} extents",
            buf.len()
        )));
    }

    let mut out = Vec::with_capacity(count as usize);
    let mut previous_end = 0u64;
    for i in 0..count as usize {
        let e = Extent::parse(&buf[i * EXTENT_RECORD_SIZE..])?;
        if e.startoff < previous_end {
            return Err(Error::BadSuperblock(format!(
                "extent {i} starts at file block {} but the previous extent ran to {previous_end}",
                e.startoff
            )));
        }
        previous_end = e.end_offset();
        out.push(e);
    }
    Ok(out)
}

/// Find the extent covering file block `off` in a sorted extent list.
///
/// Returns `None` for a hole — a file block with no extent, which reads
/// as zeros.
pub fn lookup(extents: &[Extent], off: u64) -> Option<&Extent> {
    // The list is sorted by startoff and non-overlapping, so a binary
    // search is well-defined. Files with many thousands of extents are
    // ordinary, which is why this is not a linear scan.
    let idx = extents.partition_point(|e| e.end_offset() <= off);
    extents.get(idx).filter(|e| e.contains(off))
}

#[cfg(test)]
mod tests {
    //! These fixtures are built in-process, so they prove the decoder is
    //! self-consistent and nothing more. Correctness is established by
    //! `tests/extent_oracle.rs`, which checks against the reference
    //! debugger's view of real filesystems. See AGENTS.md.

    use super::*;

    fn record(startoff: u64, startblock: u64, blockcount: u64, unwritten: bool) -> [u8; 16] {
        Extent {
            startoff,
            startblock,
            blockcount,
            unwritten,
        }
        .to_bytes()
        .unwrap()
    }

    #[test]
    fn round_trips_a_simple_extent() {
        let e = Extent::parse(&record(0, 100, 8, false)).unwrap();
        assert_eq!(e.startoff, 0);
        assert_eq!(e.startblock, 100);
        assert_eq!(e.blockcount, 8);
        assert!(!e.unwritten);
    }

    #[test]
    fn round_trips_an_unwritten_extent() {
        let e = Extent::parse(&record(4, 200, 16, true)).unwrap();
        assert!(e.is_unwritten());
        assert_eq!(e.startblock, 200);
    }

    /// `startblock` is the one field split across both halves of the
    /// record, so pin it at a value that populates bits on both sides of
    /// the boundary. A shift off by one here would still decode small
    /// block numbers correctly and fail only on large filesystems.
    #[test]
    fn decodes_a_startblock_spanning_both_halves() {
        let big = (1u64 << 50) | (1 << 43) | 1;
        let e = Extent::parse(&record(0, big, 1, false)).unwrap();
        assert_eq!(e.startblock, big);
    }

    #[test]
    fn decodes_each_field_at_its_maximum() {
        let e = Extent::parse(&record(STARTOFF_MAX, STARTBLOCK_MAX, BLOCKCOUNT_MAX, true)).unwrap();
        assert_eq!(e.startoff, STARTOFF_MAX);
        assert_eq!(e.startblock, STARTBLOCK_MAX);
        assert_eq!(e.blockcount, BLOCKCOUNT_MAX);
        assert!(e.unwritten);
    }

    /// Each field must be independent: setting one to its maximum must
    /// not bleed into its neighbours.
    #[test]
    fn fields_do_not_bleed_into_each_other() {
        let e = Extent::parse(&record(STARTOFF_MAX, 0, 1, false)).unwrap();
        assert_eq!(e.startblock, 0, "startoff bled into startblock");
        assert_eq!(e.blockcount, 1, "startoff bled into blockcount");

        let e = Extent::parse(&record(0, STARTBLOCK_MAX, 1, false)).unwrap();
        assert_eq!(e.startoff, 0, "startblock bled into startoff");
        assert_eq!(e.blockcount, 1, "startblock bled into blockcount");
    }

    #[test]
    fn rejects_a_zero_length_extent() {
        let mut b = record(0, 100, 1, false);
        b[15] = 0; // clear the blockcount
        assert!(matches!(Extent::parse(&b), Err(Error::BadSuperblock(_))));
    }

    #[test]
    fn rejects_a_short_buffer() {
        assert!(matches!(
            Extent::parse(&[0u8; 8]),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_encoding_an_oversized_field() {
        let e = Extent {
            startoff: STARTOFF_MAX + 1,
            startblock: 0,
            blockcount: 1,
            unwritten: false,
        };
        assert!(e.to_bytes().is_err());
    }

    fn list() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&record(0, 100, 4, false));
        b.extend_from_slice(&record(4, 200, 4, false));
        // Deliberate gap at file blocks 8..12 — a hole.
        b.extend_from_slice(&record(12, 300, 4, false));
        b
    }

    #[test]
    fn parses_a_list() {
        let e = parse_list(&list(), 3).unwrap();
        assert_eq!(e.len(), 3);
        assert_eq!(e[2].startoff, 12);
    }

    #[test]
    fn rejects_out_of_order_extents() {
        let mut b = Vec::new();
        b.extend_from_slice(&record(8, 100, 4, false));
        b.extend_from_slice(&record(0, 200, 4, false));
        assert!(matches!(parse_list(&b, 2), Err(Error::BadSuperblock(_))));
    }

    #[test]
    fn rejects_overlapping_extents() {
        let mut b = Vec::new();
        b.extend_from_slice(&record(0, 100, 8, false));
        b.extend_from_slice(&record(4, 200, 4, false)); // overlaps the first
        assert!(matches!(parse_list(&b, 2), Err(Error::BadSuperblock(_))));
    }

    #[test]
    fn rejects_a_count_larger_than_the_fork() {
        assert!(matches!(
            parse_list(&list(), 9),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn maps_file_blocks_to_filesystem_blocks() {
        let e = parse_list(&list(), 3).unwrap();
        assert_eq!(lookup(&e, 0).and_then(|x| x.map(0)), Some(100));
        assert_eq!(lookup(&e, 3).and_then(|x| x.map(3)), Some(103));
        assert_eq!(lookup(&e, 4).and_then(|x| x.map(4)), Some(200));
        assert_eq!(lookup(&e, 15).and_then(|x| x.map(15)), Some(303));
    }

    /// A file block with no extent is a hole and reads as zeros. It must
    /// be reported as absent rather than mapped to block 0, which is a
    /// real block holding the superblock.
    #[test]
    fn reports_holes_as_absent() {
        let e = parse_list(&list(), 3).unwrap();
        assert!(lookup(&e, 8).is_none());
        assert!(lookup(&e, 11).is_none());
        assert!(
            lookup(&e, 16).is_none(),
            "past the last extent is a hole too"
        );
    }
}
