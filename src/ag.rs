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

use crate::endian::{be32, be64, le32, uuid_at};
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

const AGF_CRC_OFFSET: usize = offsets::agf::CRC;
const AGI_CRC_OFFSET: usize = offsets::agi::CRC;

/// Byte offsets within the on-disk AGF and AGI structures.
///
/// Named for the same reason the superblock's are: a bare numeric
/// literal in a parse expression gives a reader no way to distinguish a
/// correct offset from a typo.
pub mod offsets {
    /// Fields shared by both headers, at the same offset in each.
    pub mod common {
        /// Structure magic.
        pub const MAGIC: usize = 0;
        /// Format version.
        pub const VERSIONNUM: usize = 4;
        /// Which allocation group this header describes.
        pub const SEQNO: usize = 8;
        /// Size of this AG in filesystem blocks.
        pub const LENGTH: usize = 12;
    }

    /// `xfs_agf` — free space management.
    pub mod agf {
        /// `agf_roots` — free-space B+tree roots, 3 x u32.
        pub const ROOTS: usize = 16;
        /// `agf_levels` — depths of those B+trees, 3 x u32.
        pub const LEVELS: usize = 28;
        /// `agf_flfirst` — first valid free-list entry.
        pub const FLFIRST: usize = 40;
        /// `agf_fllast` — last valid free-list entry.
        pub const FLLAST: usize = 44;
        /// `agf_flcount` — blocks currently on the free list.
        pub const FLCOUNT: usize = 48;
        /// `agf_freeblks` — total free blocks in this AG.
        pub const FREEBLKS: usize = 52;
        /// `agf_longest` — longest contiguous free extent.
        pub const LONGEST: usize = 56;
        /// `agf_btreeblks` — blocks held by the free-space B+trees.
        pub const BTREEBLKS: usize = 60;
        /// `agf_uuid` — owning filesystem. v5 only.
        pub const UUID: usize = 64;
        /// `agf_rmap_blocks` — blocks held by the reverse-map B+tree.
        pub const RMAP_BLOCKS: usize = 80;
        /// `agf_refcount_root` — reference-count B+tree root.
        pub const REFCOUNT_ROOT: usize = 88;
        /// `agf_refcount_level` — reference-count B+tree depth.
        pub const REFCOUNT_LEVEL: usize = 92;
        /// `agf_lsn` — log sequence number of the last write. v5 only.
        pub const LSN: usize = 208;
        /// `agf_crc` — CRC32C, stored little-endian. v5 only.
        pub const CRC: usize = 216;
    }

    /// `xfs_agi` — inode management.
    pub mod agi {
        /// `agi_count` — allocated inodes in this AG.
        pub const COUNT: usize = 16;
        /// `agi_root` — inode B+tree root.
        pub const ROOT: usize = 20;
        /// `agi_level` — inode B+tree depth.
        pub const LEVEL: usize = 24;
        /// `agi_freecount` — free inodes in this AG.
        pub const FREECOUNT: usize = 28;
        /// `agi_newino` — most recently allocated inode chunk.
        pub const NEWINO: usize = 32;
        /// `agi_dirino` — unused in modern XFS.
        pub const DIRINO: usize = 36;
        /// `agi_unlinked` — hash buckets of unlinked-but-open inodes.
        pub const UNLINKED: usize = 40;
        /// `agi_uuid` — owning filesystem. v5 only.
        pub const UUID: usize = 296;
        /// `agi_crc` — CRC32C, stored little-endian. v5 only.
        pub const CRC: usize = 312;
        /// `agi_lsn` — log sequence number of the last write. v5 only.
        pub const LSN: usize = 320;
        /// `agi_free_root` — free-inode B+tree root.
        pub const FREE_ROOT: usize = 328;
        /// `agi_free_level` — free-inode B+tree depth.
        pub const FREE_LEVEL: usize = 332;
    }
}

/// Index of each free-space B+tree root within `agf_roots` / `agf_levels`.
pub mod agf_btree {
    /// B+tree indexed by block number.
    pub const BNO: usize = 0;
    /// B+tree indexed by extent length.
    pub const CNT: usize = 1;
    /// Reverse-mapping B+tree (only present with the `rmapbt` feature).
    pub const RMAP: usize = 2;
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
    let uuid = uuid_at(buf, uuid_off);
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
        let magic = be32(buf, offsets::common::MAGIC);
        if magic != XFS_AGF_MAGIC {
            return Err(Error::BadSuperblock(format!(
                "AGF for ag {expected_ag} has magic {magic:#010x}, expected {XFS_AGF_MAGIC:#010x}"
            )));
        }
        if sb.is_v5() {
            verify_crc(
                "AGF",
                buf,
                usize::from(sb.sectsize),
                AGF_CRC_OFFSET,
                expected_ag,
            )?;
            check_identity(
                "AGF",
                buf,
                sb,
                offsets::agf::UUID,
                offsets::common::SEQNO,
                expected_ag,
            )?;
        }

        let agf = Agf {
            seqno: be32(buf, offsets::common::SEQNO),
            length: be32(buf, offsets::common::LENGTH),
            roots: [
                be32(buf, offsets::agf::ROOTS),
                be32(buf, offsets::agf::ROOTS + 4),
                be32(buf, offsets::agf::ROOTS + 8),
            ],
            levels: [
                be32(buf, offsets::agf::LEVELS),
                be32(buf, offsets::agf::LEVELS + 4),
                be32(buf, offsets::agf::LEVELS + 8),
            ],
            flfirst: be32(buf, offsets::agf::FLFIRST),
            fllast: be32(buf, offsets::agf::FLLAST),
            flcount: be32(buf, offsets::agf::FLCOUNT),
            freeblks: be32(buf, offsets::agf::FREEBLKS),
            longest: be32(buf, offsets::agf::LONGEST),
            btreeblks: be32(buf, offsets::agf::BTREEBLKS),
            rmap_blocks: be32(buf, offsets::agf::RMAP_BLOCKS),
            refcount_root: be32(buf, offsets::agf::REFCOUNT_ROOT),
            refcount_level: be32(buf, offsets::agf::REFCOUNT_LEVEL),
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
        let magic = be32(buf, offsets::common::MAGIC);
        if magic != XFS_AGI_MAGIC {
            return Err(Error::BadSuperblock(format!(
                "AGI for ag {expected_ag} has magic {magic:#010x}, expected {XFS_AGI_MAGIC:#010x}"
            )));
        }
        if sb.is_v5() {
            verify_crc(
                "AGI",
                buf,
                usize::from(sb.sectsize),
                AGI_CRC_OFFSET,
                expected_ag,
            )?;
            check_identity(
                "AGI",
                buf,
                sb,
                offsets::agi::UUID,
                offsets::common::SEQNO,
                expected_ag,
            )?;
        }

        let mut unlinked = [0u32; XFS_AGI_UNLINKED_BUCKETS];
        for (i, slot) in unlinked.iter_mut().enumerate() {
            *slot = be32(buf, offsets::agi::UNLINKED + i * 4);
        }

        let agi = Agi {
            seqno: be32(buf, offsets::common::SEQNO),
            length: be32(buf, offsets::common::LENGTH),
            count: be32(buf, offsets::agi::COUNT),
            root: be32(buf, offsets::agi::ROOT),
            level: be32(buf, offsets::agi::LEVEL),
            freecount: be32(buf, offsets::agi::FREECOUNT),
            newino: be32(buf, offsets::agi::NEWINO),
            free_root: be32(buf, offsets::agi::FREE_ROOT),
            free_level: be32(buf, offsets::agi::FREE_LEVEL),
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
    // Checksums are little-endian; see `superblock::le32`.
    let stored = le32(buf, crc_off);
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
    be64(buf, offsets::agf::LSN)
}

/// Read the log sequence number from a v5 AGI.
pub fn agi_lsn(buf: &[u8]) -> u64 {
    be64(buf, offsets::agi::LSN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal v5 superblock matching the geometry the AG fixtures use.
    fn sb_v5() -> Superblock {
        let mut b = vec![0u8; 512];
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
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).unwrap()
    }

    fn build_agf(sb: &Superblock, ag: u32) -> Vec<u8> {
        let mut b = vec![0u8; 512];
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
        b[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        b
    }

    fn build_agi(sb: &Superblock, ag: u32) -> Vec<u8> {
        let mut b = vec![0u8; 512];
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
        b[AGI_CRC_OFFSET..AGI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
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
        buf[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
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
        buf[AGF_CRC_OFFSET..AGF_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
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
        buf[AGI_CRC_OFFSET..AGI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        let agi = Agi::parse(&buf, &sb, 0).unwrap();
        assert!(agi.has_unlinked_inodes());
    }
}
