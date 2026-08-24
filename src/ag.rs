//! Allocation group headers: AGF, AGI and the AG free list.
//!
//! XFS divides the data section into equally-sized allocation groups.
//! Each AG begins with four sectors: the superblock, the AGF (free space
//! management), the AGI (inode management), and the AGFL (a small
//! reserve of blocks the free-space B+trees can consume while modifying
//! themselves).
//!
//! # Self-describing metadata
//!
//! On v5 every metadata block carries, in addition to a CRC32C:
//!
//! - the **filesystem UUID** it belongs to,
//! - the **AG number** it belongs to,
//! - a **log sequence number** recording when it was last written.
//!
//! That redundancy is worth more than the checksum alone. A CRC detects
//! corrupted bits; the identity fields detect a *correct* block that
//! ended up in the wrong place — a misdirected write, a stale block
//! resurrected from an earlier filesystem, or a reader computing the
//! wrong address. Those failures are invisible to a checksum, because
//! the block is internally perfect. Every header parsed here is checked
//! against the address it was read from.

use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// `XAGF` — AGF magic.
pub const XFS_AGF_MAGIC: u32 = 0x5841_4746;
/// `XAGI` — AGI magic.
pub const XFS_AGI_MAGIC: u32 = 0x5841_4749;
/// `XAFL` — AGFL magic (v5 only; v4 AGFLs have no header).
pub const XFS_AGFL_MAGIC: u32 = 0x5841_464c;

/// On-disk size of the AGF structure.
pub const XFS_AGF_SIZE: usize = 224;
/// On-disk size of the AGI structure.
pub const XFS_AGI_SIZE: usize = 344;

/// Number of unlinked-inode hash buckets in the AGI.
pub const XFS_AGI_UNLINKED_BUCKETS: usize = 64;

const AGF_CRC_OFFSET: usize = 216;
const AGI_CRC_OFFSET: usize = 312;

/// Index of each free-space B+tree root within `agf_roots` / `agf_levels`.
pub mod agf_btree {
    /// B+tree indexed by block number.
    pub const BNO: usize = 0;
    /// B+tree indexed by extent length.
    pub const CNT: usize = 1;
    /// Reverse-mapping B+tree (only present with the `rmapbt` feature).
    pub const RMAP: usize = 2;
}

#[inline]
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[inline]
fn be64(b: &[u8], off: usize) -> u64 {
    u64::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// Shared identity validation for a v5 metadata header.
///
/// `uuid_off` and `seqno_off` are the byte offsets of the UUID and the
/// owning-AG fields within the structure. Returns an error when the
/// block does not belong to this filesystem or claims a different AG
/// than the one it was read from.
fn check_identity(
    what: &'static str,
    buf: &[u8],
    sb: &Superblock,
    uuid_off: usize,
    seqno_off: usize,
    expected_ag: u32,
) -> Result<()> {
    let uuid: [u8; 16] = buf[uuid_off..uuid_off + 16].try_into().expect("16 bytes");
    if uuid != sb.meta_uuid {
        return Err(Error::BlockIdentityMismatch {
            what,
            expected: u64::from(expected_ag),
            found: u64::MAX, // UUID mismatch: the AG number is not meaningful
        });
    }
    let seqno = be32(buf, seqno_off);
    if seqno != expected_ag {
        return Err(Error::BlockIdentityMismatch {
            what,
            expected: u64::from(expected_ag),
            found: u64::from(seqno),
        });
    }
    Ok(())
}

/// AGF — per-AG free space management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agf {
    /// Which allocation group this header describes.
    pub seqno: u32,
    /// Size of this AG in filesystem blocks. The final AG is usually
    /// shorter than `sb_agblocks`.
    pub length: u32,
    /// Root blocks of the free-space B+trees, indexed by [`agf_btree`].
    pub roots: [u32; 3],
    /// Depths of those B+trees. A level of 1 means the root is a leaf.
    pub levels: [u32; 3],
    /// Index of the first valid AGFL entry.
    pub flfirst: u32,
    /// Index of the last valid AGFL entry.
    pub fllast: u32,
    /// Number of blocks currently on the free list.
    pub flcount: u32,
    /// Total free blocks in this AG.
    pub freeblks: u32,
    /// Longest contiguous free extent in this AG.
    pub longest: u32,
    /// Blocks held by the free-space B+trees themselves.
    pub btreeblks: u32,
    /// Blocks held by the reverse-mapping B+tree.
    pub rmap_blocks: u32,
    /// Root block of the reference-count B+tree (reflink only).
    pub refcount_root: u32,
    /// Depth of the reference-count B+tree.
    pub refcount_level: u32,
}

impl Agf {
    /// Parse and validate an AGF read from AG `expected_ag`.
    pub fn parse(buf: &[u8], sb: &Superblock, expected_ag: u32) -> Result<Self> {
        if buf.len() < XFS_AGF_SIZE {
            return Err(Error::BadSuperblock(format!(
                "AGF needs {XFS_AGF_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        let magic = be32(buf, 0);
        if magic != XFS_AGF_MAGIC {
            return Err(Error::BadSuperblock(format!(
                "AGF for ag {expected_ag} has magic {magic:#010x}, expected {XFS_AGF_MAGIC:#010x}"
            )));
        }
        if sb.is_v5() {
            verify_crc("AGF", buf, XFS_AGF_SIZE, AGF_CRC_OFFSET, expected_ag)?;
            check_identity("AGF", buf, sb, 64, 8, expected_ag)?;
        }

        let agf = Agf {
            seqno: be32(buf, 8),
            length: be32(buf, 12),
            roots: [be32(buf, 16), be32(buf, 20), be32(buf, 24)],
            levels: [be32(buf, 28), be32(buf, 32), be32(buf, 36)],
            flfirst: be32(buf, 40),
            fllast: be32(buf, 44),
            flcount: be32(buf, 48),
            freeblks: be32(buf, 52),
            longest: be32(buf, 56),
            btreeblks: be32(buf, 60),
            rmap_blocks: be32(buf, 80),
            refcount_root: be32(buf, 88),
            refcount_level: be32(buf, 92),
        };

        // An AG can never be longer than the geometry the superblock
        // declares, and a free-space count larger than the AG itself is
        // the classic symptom of reading the wrong offset.
        if agf.length > sb.agblocks {
            return Err(Error::BadSuperblock(format!(
                "AGF ag {expected_ag} length {} exceeds sb_agblocks {}",
                agf.length, sb.agblocks
            )));
        }
        if agf.freeblks > agf.length {
            return Err(Error::BadSuperblock(format!(
                "AGF ag {expected_ag} freeblks {} exceeds its own length {}",
                agf.freeblks, agf.length
            )));
        }
        if agf.longest > agf.length {
            return Err(Error::BadSuperblock(format!(
                "AGF ag {expected_ag} longest extent {} exceeds its own length {}",
                agf.longest, agf.length
            )));
        }
        Ok(agf)
    }

    /// Whether the reverse-mapping B+tree is populated for this AG.
    pub fn has_rmap(&self) -> bool {
        self.levels[agf_btree::RMAP] > 0
    }
}

/// AGI — per-AG inode management.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agi {
    /// Which allocation group this header describes.
    pub seqno: u32,
    /// Size of this AG in filesystem blocks.
    pub length: u32,
    /// Allocated inodes in this AG.
    pub count: u32,
    /// Root block of the inode B+tree.
    pub root: u32,
    /// Depth of the inode B+tree.
    pub level: u32,
    /// Free inodes in this AG.
    pub freecount: u32,
    /// Most recently allocated inode chunk.
    pub newino: u32,
    /// Root block of the free-inode B+tree (`finobt`), if present.
    pub free_root: u32,
    /// Depth of the free-inode B+tree.
    pub free_level: u32,
    /// Hash buckets heading the lists of inodes that are unlinked but
    /// still open. A non-empty bucket after an unclean shutdown means
    /// there are inodes to reclaim during recovery.
    pub unlinked: [u32; XFS_AGI_UNLINKED_BUCKETS],
}

impl Agi {
    /// Parse and validate an AGI read from AG `expected_ag`.
    pub fn parse(buf: &[u8], sb: &Superblock, expected_ag: u32) -> Result<Self> {
        if buf.len() < XFS_AGI_SIZE {
            return Err(Error::BadSuperblock(format!(
                "AGI needs {XFS_AGI_SIZE} bytes, got {}",
                buf.len()
            )));
        }
        let magic = be32(buf, 0);
        if magic != XFS_AGI_MAGIC {
            return Err(Error::BadSuperblock(format!(
                "AGI for ag {expected_ag} has magic {magic:#010x}, expected {XFS_AGI_MAGIC:#010x}"
            )));
        }
        if sb.is_v5() {
            verify_crc("AGI", buf, XFS_AGI_SIZE, AGI_CRC_OFFSET, expected_ag)?;
            check_identity("AGI", buf, sb, 296, 8, expected_ag)?;
        }

        let mut unlinked = [0u32; XFS_AGI_UNLINKED_BUCKETS];
        for (i, slot) in unlinked.iter_mut().enumerate() {
            *slot = be32(buf, 40 + i * 4);
        }

        let agi = Agi {
            seqno: be32(buf, 8),
            length: be32(buf, 12),
            count: be32(buf, 16),
            root: be32(buf, 20),
            level: be32(buf, 24),
            freecount: be32(buf, 28),
            newino: be32(buf, 32),
            free_root: be32(buf, 328),
            free_level: be32(buf, 332),
            unlinked,
        };

        if agi.freecount > agi.count {
            return Err(Error::BadSuperblock(format!(
                "AGI ag {expected_ag} freecount {} exceeds count {}",
                agi.freecount, agi.count
            )));
        }
        if agi.level == 0 {
            return Err(Error::BadSuperblock(format!(
                "AGI ag {expected_ag} inode btree has level 0"
            )));
        }
        Ok(agi)
    }

    /// Whether any inode in this AG is unlinked but still referenced.
    ///
    /// XFS marks a bucket empty with `NULLAGINO` (`u32::MAX`), not zero.
    pub fn has_unlinked_inodes(&self) -> bool {
        self.unlinked.iter().any(|&b| b != u32::MAX)
    }
}

/// Verify the CRC32C of a v5 metadata block.
fn verify_crc(what: &'static str, buf: &[u8], size: usize, crc_off: usize, ag: u32) -> Result<()> {
    let stored = be32(buf, crc_off);
    let computed = crc32c_with_zeroed_crc(&buf[..size], crc_off);
    if stored != computed {
        return Err(Error::ChecksumMismatch {
            what,
            block: u64::from(ag),
        });
    }
    Ok(())
}

/// Read the log sequence number from a v5 AGF.
pub fn agf_lsn(buf: &[u8]) -> u64 {
    be64(buf, 208)
}

/// Read the log sequence number from a v5 AGI.
pub fn agi_lsn(buf: &[u8]) -> u64 {
    be64(buf, 320)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::XFS_SB_SIZE;

    /// Minimal v5 superblock matching the geometry the AG fixtures use.
    fn sb_v5() -> Superblock {
        let mut b = vec![0u8; XFS_SB_SIZE];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes());
        b[8..16].copy_from_slice(&4000u64.to_be_bytes());
        b[48..56].copy_from_slice(&100u64.to_be_bytes());
        b[56..64].copy_from_slice(&128u64.to_be_bytes());
        b[84..88].copy_from_slice(&1000u32.to_be_bytes());
        b[88..92].copy_from_slice(&4u32.to_be_bytes());
        b[100..102].copy_from_slice(&5u16.to_be_bytes());
        b[102..104].copy_from_slice(&512u16.to_be_bytes());
        b[104..106].copy_from_slice(&512u16.to_be_bytes());
        b[106..108].copy_from_slice(&8u16.to_be_bytes());
        b[120] = 12;
        b[121] = 9;
        b[122] = 9;
        b[123] = 3;
        b[124] = 10;
        // Distinctive UUID so identity mismatches are unambiguous.
        for (i, slot) in b[32..48].iter_mut().enumerate() {
            *slot = i as u8;
        }
        let crc = crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_be_bytes());
        Superblock::parse(&b).unwrap()
    }

    fn build_agf(sb: &Superblock, ag: u32) -> Vec<u8> {
        let mut b = vec![0u8; XFS_AGF_SIZE];
        b[0..4].copy_from_slice(&XFS_AGF_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&1u32.to_be_bytes()); // versionnum
        b[8..12].copy_from_slice(&ag.to_be_bytes()); // seqno
        b[12..16].copy_from_slice(&1000u32.to_be_bytes()); // length
        b[16..20].copy_from_slice(&1u32.to_be_bytes()); // roots[BNO]
        b[20..24].copy_from_slice(&2u32.to_be_bytes()); // roots[CNT]
        b[28..32].copy_from_slice(&1u32.to_be_bytes()); // levels[BNO]
        b[32..36].copy_from_slice(&1u32.to_be_bytes()); // levels[CNT]
        b[52..56].copy_from_slice(&900u32.to_be_bytes()); // freeblks
        b[56..60].copy_from_slice(&800u32.to_be_bytes()); // longest
        b[64..80].copy_from_slice(&sb.meta_uuid);
        let crc = crc32c_with_zeroed_crc(&b, AGF_CRC_OFFSET);
        b[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
        b
    }

    fn build_agi(sb: &Superblock, ag: u32) -> Vec<u8> {
        let mut b = vec![0u8; XFS_AGI_SIZE];
        b[0..4].copy_from_slice(&XFS_AGI_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&1u32.to_be_bytes());
        b[8..12].copy_from_slice(&ag.to_be_bytes());
        b[12..16].copy_from_slice(&1000u32.to_be_bytes());
        b[16..20].copy_from_slice(&64u32.to_be_bytes()); // count
        b[20..24].copy_from_slice(&3u32.to_be_bytes()); // root
        b[24..28].copy_from_slice(&1u32.to_be_bytes()); // level
        b[28..32].copy_from_slice(&60u32.to_be_bytes()); // freecount
                                                         // Empty unlinked buckets are NULLAGINO, not zero.
        for i in 0..XFS_AGI_UNLINKED_BUCKETS {
            b[40 + i * 4..44 + i * 4].copy_from_slice(&u32::MAX.to_be_bytes());
        }
        b[296..312].copy_from_slice(&sb.meta_uuid);
        let crc = crc32c_with_zeroed_crc(&b, AGI_CRC_OFFSET);
        b[AGI_CRC_OFFSET..AGI_CRC_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
        b
    }

    #[test]
    fn parses_agf() {
        let sb = sb_v5();
        let agf = Agf::parse(&build_agf(&sb, 2), &sb, 2).unwrap();
        assert_eq!(agf.seqno, 2);
        assert_eq!(agf.roots[agf_btree::BNO], 1);
        assert_eq!(agf.roots[agf_btree::CNT], 2);
        assert_eq!(agf.freeblks, 900);
        assert!(!agf.has_rmap());
    }

    #[test]
    fn parses_agi() {
        let sb = sb_v5();
        let agi = Agi::parse(&build_agi(&sb, 1), &sb, 1).unwrap();
        assert_eq!(agi.seqno, 1);
        assert_eq!(agi.count, 64);
        assert_eq!(agi.freecount, 60);
        assert!(!agi.has_unlinked_inodes());
    }

    /// The whole point of the self-describing header: a block that is
    /// internally perfect but came from the wrong AG must be rejected.
    /// A CRC alone cannot catch this.
    #[test]
    fn rejects_agf_from_wrong_ag() {
        let sb = sb_v5();
        let buf = build_agf(&sb, 2); // valid AGF, but for AG 2
        match Agf::parse(&buf, &sb, 3) {
            Err(Error::BlockIdentityMismatch {
                expected, found, ..
            }) => {
                assert_eq!(expected, 3);
                assert_eq!(found, 2);
            }
            other => panic!("expected identity mismatch, got {other:?}"),
        }
    }

    #[test]
    fn rejects_agi_from_wrong_ag() {
        let sb = sb_v5();
        let buf = build_agi(&sb, 0);
        assert!(matches!(
            Agi::parse(&buf, &sb, 1),
            Err(Error::BlockIdentityMismatch { .. })
        ));
    }

    /// A block belonging to a different filesystem entirely — a stale
    /// block left behind by a previous mkfs on the same device.
    #[test]
    fn rejects_agf_from_foreign_filesystem() {
        let sb = sb_v5();
        let mut buf = build_agf(&sb, 0);
        buf[64] ^= 0xFF; // corrupt the UUID
        let crc = crc32c_with_zeroed_crc(&buf, AGF_CRC_OFFSET);
        buf[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            Agf::parse(&buf, &sb, 0),
            Err(Error::BlockIdentityMismatch { .. })
        ));
    }

    #[test]
    fn rejects_bad_agf_crc() {
        let sb = sb_v5();
        let mut buf = build_agf(&sb, 0);
        buf[52] ^= 0xFF; // flip a byte without fixing the CRC
        assert!(matches!(
            Agf::parse(&buf, &sb, 0),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_agf_with_impossible_freeblks() {
        let sb = sb_v5();
        let mut buf = build_agf(&sb, 0);
        buf[52..56].copy_from_slice(&5000u32.to_be_bytes()); // > length
        let crc = crc32c_with_zeroed_crc(&buf, AGF_CRC_OFFSET);
        buf[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
        assert!(matches!(
            Agf::parse(&buf, &sb, 0),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_wrong_magic() {
        let sb = sb_v5();
        let mut buf = build_agf(&sb, 0);
        buf[0..4].copy_from_slice(&XFS_AGI_MAGIC.to_be_bytes()); // AGI magic in an AGF
        assert!(matches!(
            Agf::parse(&buf, &sb, 0),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn detects_unlinked_inodes() {
        let sb = sb_v5();
        let mut buf = build_agi(&sb, 0);
        buf[40..44].copy_from_slice(&42u32.to_be_bytes()); // bucket 0 occupied
        let crc = crc32c_with_zeroed_crc(&buf, AGI_CRC_OFFSET);
        buf[AGI_CRC_OFFSET..AGI_CRC_OFFSET + 4].copy_from_slice(&crc.to_be_bytes());
        let agi = Agi::parse(&buf, &sb, 0).unwrap();
        assert!(agi.has_unlinked_inodes());
    }
}
