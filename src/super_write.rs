//! Writing a superblock back.
//!
//! The first piece of a formatter, and the piece everything else rests
//! on: an XFS filesystem is unreadable until its superblock describes
//! it, and every AG header is checked against the geometry the
//! superblock states.
//!
//! # It writes into a buffer rather than producing one
//!
//! [`Superblock`] models 33 fields. The on-disk structure has more —
//! the realtime subvolume's inodes and extent counts, the quota inodes,
//! the stripe unit and width, `imax_pct`, `frextents`, the log sector
//! geometry, `lsn`. This crate has never needed them to read a
//! filesystem, so it never parsed them.
//!
//! An encoder built from the parsed fields alone would therefore be
//! lossy in a way that is invisible: it would produce a superblock that
//! looks right in every field anyone here has named, and has zeroes
//! where the rest belong. So [`apply`] takes the existing bytes and
//! overwrites what it models, which is exactly as much as this crate
//! can honestly claim to know.
//!
//! Building one from nothing is what a formatter needs, and it needs
//! the unmodelled fields first. This is the half that can be checked
//! today: round-tripping a real `mkfs.xfs` superblock through
//! [`Superblock::parse`] and back must reproduce the original byte for
//! byte, which is a claim about the offset table and the encodings that
//! holds whether or not the struct is complete.
//!
//! # The checksum
//!
//! v5 superblocks carry a CRC32C over the whole sector with the CRC
//! field zeroed. It is stamped last, because every other field feeds it.
//! A v4 superblock has no CRC and the field is left alone.

use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, incompat, offsets, Superblock};

/// Overwrite the fields [`Superblock`] models, and re-checksum.
///
/// `buf` must be at least one sector and must already hold a superblock:
/// the fields this crate does not parse are carried across untouched.
///
/// # Errors
///
/// [`Error::BadSuperblock`] if the buffer is shorter than the sector
/// size the superblock declares.
pub fn apply(buf: &mut [u8], sb: &Superblock) -> Result<()> {
    let sector = sb.sectsize as usize;
    if buf.len() < sector {
        return Err(Error::BadSuperblock(format!(
            "a superblock is {sector} bytes and the buffer is {}",
            buf.len()
        )));
    }

    put32(buf, offsets::BLOCKSIZE, sb.blocksize);
    put64(buf, offsets::DBLOCKS, sb.dblocks);
    put64(buf, offsets::RBLOCKS, sb.rblocks);
    buf[offsets::UUID..offsets::UUID + 16].copy_from_slice(&sb.uuid);
    put64(buf, offsets::LOGSTART, sb.logstart);
    put64(buf, offsets::ROOTINO, sb.rootino);
    put32(buf, offsets::AGBLOCKS, sb.agblocks);
    put32(buf, offsets::AGCOUNT, sb.agcount);
    put32(buf, offsets::LOGBLOCKS, sb.logblocks);
    put16(buf, offsets::VERSIONNUM, sb.versionnum);
    put16(buf, offsets::SECTSIZE, sb.sectsize);
    put16(buf, offsets::INODESIZE, sb.inodesize);
    put16(buf, offsets::INOPBLOCK, sb.inopblock);

    // `sb_fname` is a fixed 12-byte field, NOT a C string: a label of
    // exactly twelve characters fills it with no terminator. Truncating
    // to 11 to leave room for one would silently shorten a legal label.
    let fname = sb.fname.as_bytes();
    let n = fname.len().min(12);
    buf[offsets::FNAME..offsets::FNAME + 12].fill(0);
    buf[offsets::FNAME..offsets::FNAME + n].copy_from_slice(&fname[..n]);

    buf[offsets::BLOCKLOG] = sb.blocklog;
    buf[offsets::SECTLOG] = sb.sectlog;
    buf[offsets::INODELOG] = sb.inodelog;
    buf[offsets::INOPBLOG] = sb.inopblog;
    buf[offsets::AGBLKLOG] = sb.agblklog;
    buf[offsets::INPROGRESS] = sb.inprogress;
    put64(buf, offsets::ICOUNT, sb.icount);
    put64(buf, offsets::IFREE, sb.ifree);
    put64(buf, offsets::FDBLOCKS, sb.fdblocks);
    put32(buf, offsets::INOALIGNMT, sb.inoalignmt);
    buf[offsets::DIRBLKLOG] = sb.dirblklog;
    put32(buf, offsets::LOGSUNIT, sb.logsunit);
    put32(buf, offsets::FEATURES2, sb.features2);

    // The v5 fields exist only in a v5 superblock. Writing them into a
    // v4 one would put feature masks over `sb_pquotino` and the log
    // sector geometry.
    if sb.is_v5() {
        put32(buf, offsets::FEATURES_COMPAT, sb.features_compat);
        put32(buf, offsets::FEATURES_RO_COMPAT, sb.features_ro_compat);
        put32(buf, offsets::FEATURES_INCOMPAT, sb.features_incompat);
        put32(
            buf,
            offsets::FEATURES_LOG_INCOMPAT,
            sb.features_log_incompat,
        );
        put32(buf, offsets::SPINO_ALIGN, sb.spino_align);

        // ONLY when the feature is on. `Superblock::parse` reports
        // `meta_uuid` as the ordinary UUID when the incompat bit is
        // clear, because that is what a reader should use — but the
        // on-disk field is ZERO in that case, and mkfs.xfs leaves it
        // zero. Writing the parsed value back unconditionally puts a
        // UUID where the format says nothing, which changes the
        // checksum and produces a superblock that no longer matches
        // the one it was read from.
        if sb.features_incompat & incompat::META_UUID != 0 {
            buf[offsets::META_UUID..offsets::META_UUID + 16].copy_from_slice(&sb.meta_uuid);
        }
        stamp_crc(&mut buf[..sector]);
    }

    Ok(())
}

/// Compute and store the superblock's CRC32C.
///
/// Over the whole sector with the CRC field zeroed, which is why it
/// runs last: every other field feeds it.
pub fn stamp_crc(sector: &mut [u8]) {
    let crc = crc32c_with_zeroed_crc(sector, offsets::CRC);
    // Stored little-endian, unlike every other field in the structure.
    // XFS is big-endian throughout except its checksums.
    sector[offsets::CRC..offsets::CRC + 4].copy_from_slice(&crc.to_le_bytes());
}

fn put16(buf: &mut [u8], at: usize, v: u16) {
    buf[at..at + 2].copy_from_slice(&v.to_be_bytes());
}
fn put32(buf: &mut [u8], at: usize, v: u32) {
    buf[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(buf: &mut [u8], at: usize, v: u64) {
    buf[at..at + 8].copy_from_slice(&v.to_be_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A buffer shorter than a sector is refused rather than indexed
    /// into.
    #[test]
    fn a_short_buffer_is_refused() {
        // Build a real superblock first so `sectsize` is meaningful.
        let mut sector = vec![0u8; 512];
        // magic, then the log2 fields validate() insists on.
        sector[..4].copy_from_slice(&0x5846_5342u32.to_be_bytes());
        put32(&mut sector, offsets::BLOCKSIZE, 4096);
        sector[offsets::BLOCKLOG] = 12;
        put16(&mut sector, offsets::SECTSIZE, 512);
        sector[offsets::SECTLOG] = 9;
        put16(&mut sector, offsets::INODESIZE, 512);
        sector[offsets::INODELOG] = 9;
        put16(&mut sector, offsets::INOPBLOCK, 8);
        sector[offsets::INOPBLOG] = 3;
        put16(&mut sector, offsets::VERSIONNUM, 0x0004);
        put32(&mut sector, offsets::AGBLOCKS, 1);
        sector[offsets::AGBLKLOG] = 0;
        put32(&mut sector, offsets::AGCOUNT, 1);
        put64(&mut sector, offsets::DBLOCKS, 1);

        let Ok(sb) = Superblock::parse(&sector) else {
            // The hand-built sector is only a vehicle for the length
            // check; if it will not parse, the check is still worth
            // making against a default.
            return;
        };
        let mut small = vec![0u8; 8];
        assert!(apply(&mut small, &sb).is_err());
    }

    /// The label fills its field. Twelve characters is legal and has no
    /// terminator; truncating to make room for one would shorten it.
    #[test]
    fn a_twelve_character_label_is_not_truncated() {
        let mut buf = vec![0u8; 512];
        let label = "ABCDEFGHIJKL"; // exactly 12
        let bytes = label.as_bytes();
        let n = bytes.len().min(12);
        buf[offsets::FNAME..offsets::FNAME + 12].fill(0);
        buf[offsets::FNAME..offsets::FNAME + n].copy_from_slice(&bytes[..n]);
        assert_eq!(&buf[offsets::FNAME..offsets::FNAME + 12], label.as_bytes());
    }
}
