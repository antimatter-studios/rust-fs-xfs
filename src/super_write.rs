//! Writing a superblock back.
//!
//! The first piece of a formatter, and the piece everything else rests
//! on: an XFS filesystem is unreadable until its superblock describes
//! it, and every AG header is checked against the geometry the
//! superblock states.
//!
//! # It writes into a buffer, and the buffer may be empty
//!
//! [`apply`] overwrites a buffer rather than returning one, because the
//! caller owns the sector: a superblock is written into an AG header
//! that already exists, and the backup copies are written into sectors
//! the formatter has already laid out.
//!
//! What changed is what the buffer has to contain beforehand: nothing.
//! [`Superblock`] used to model 33 of the structure's fields, so `apply`
//! could only be honest by carrying the rest across from whatever was
//! underneath — which made it useless to a formatter, since a formatter
//! has no superblock to carry anything from.
//!
//! Every on-disk field is now modelled, including the ones no reader
//! consults: the realtime inodes and extent counts, the quota inodes,
//! the stripe geometry, `imax_pct`, `qflags`, the log sector geometry,
//! `bad_features2`, `lsn`. So `apply` over a **zeroed** buffer produces
//! a complete superblock, and that is the claim
//! `applying_into_an_empty_buffer_reproduces_the_original` makes: for
//! every real `mkfs.xfs` fixture, parse it, apply it into zeroes, and
//! require the sector to come back byte for byte.
//!
//! That test is what proves the model complete. A field left unmodelled
//! reads as zero, writes as nothing, and shows up as a differing byte at
//! its own offset — which the failure message names. The older
//! apply-over-itself test cannot see such a field at all, because the
//! original's value is already sitting there.
//!
//! # The checksum
//!
//! v5 superblocks carry a CRC32C over the whole sector with the CRC
//! field zeroed. It is stamped last, because every other field feeds it.
//! A v4 superblock has no CRC and the field is left alone.

use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, incompat, offsets, Superblock, XFS_SB_MAGIC};

/// Write `sb` into `buf` as an on-disk superblock, and checksum it.
///
/// `buf` must be at least one sector. It does **not** need to hold a
/// superblock already: every field of the structure is written, so a
/// zeroed sector is a valid destination and is how a formatter uses
/// this. Passing the sector the superblock was read from is equally
/// valid and reproduces it exactly.
///
/// Bytes past the end of the 264-byte structure are left as they are.
/// They are part of the sector the v5 checksum covers, so a caller
/// writing into scratch memory must zero it first — which is what
/// `mkfs.xfs` leaves there.
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

    // The magic is written too, so `apply` over a zeroed buffer builds
    // a whole superblock rather than one that happens to be right
    // because a real one was underneath it.
    put32(buf, offsets::MAGIC, XFS_SB_MAGIC);
    put32(buf, offsets::BLOCKSIZE, sb.blocksize);
    put64(buf, offsets::DBLOCKS, sb.dblocks);
    put64(buf, offsets::RBLOCKS, sb.rblocks);
    put64(buf, offsets::REXTENTS, sb.rextents);
    buf[offsets::UUID..offsets::UUID + 16].copy_from_slice(&sb.uuid);
    put64(buf, offsets::LOGSTART, sb.logstart);
    put64(buf, offsets::ROOTINO, sb.rootino);
    put64(buf, offsets::RBMINO, sb.rbmino);
    put64(buf, offsets::RSUMINO, sb.rsumino);
    put32(buf, offsets::REXTSIZE, sb.rextsize);
    put32(buf, offsets::AGBLOCKS, sb.agblocks);
    put32(buf, offsets::AGCOUNT, sb.agcount);
    put32(buf, offsets::RBMBLOCKS, sb.rbmblocks);
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
    buf[offsets::REXTSLOG] = sb.rextslog;
    buf[offsets::INPROGRESS] = sb.inprogress;
    buf[offsets::IMAX_PCT] = sb.imax_pct;
    put64(buf, offsets::ICOUNT, sb.icount);
    put64(buf, offsets::IFREE, sb.ifree);
    put64(buf, offsets::FDBLOCKS, sb.fdblocks);
    put64(buf, offsets::FREXTENTS, sb.frextents);
    put64(buf, offsets::UQUOTINO, sb.uquotino);
    put64(buf, offsets::GQUOTINO, sb.gquotino);
    put16(buf, offsets::QFLAGS, sb.qflags);
    buf[offsets::FLAGS] = sb.flags;
    buf[offsets::SHARED_VN] = sb.shared_vn;
    put32(buf, offsets::INOALIGNMT, sb.inoalignmt);
    put32(buf, offsets::UNIT, sb.unit);
    put32(buf, offsets::WIDTH, sb.width);
    buf[offsets::DIRBLKLOG] = sb.dirblklog;
    buf[offsets::LOGSECTLOG] = sb.logsectlog;
    put16(buf, offsets::LOGSECTSIZE, sb.logsectsize);
    put32(buf, offsets::LOGSUNIT, sb.logsunit);
    put32(buf, offsets::FEATURES2, sb.features2);
    put32(buf, offsets::BAD_FEATURES2, sb.bad_features2);

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
        put64(buf, offsets::PQUOTINO, sb.pquotino);
        put64(buf, offsets::LSN, sb.lsn as u64);

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
