//! XFS superblock parsing and validation.
//!
//! # Byte order
//!
//! **XFS stores every multi-byte on-disk field in big-endian order**,
//! regardless of the host. This is the single most important fact about
//! reading XFS and the one that separates it from ext4, Btrfs and NTFS,
//! which are all little-endian. Every integer read in this crate goes
//! through a `from_be_bytes`, never `from_le_bytes` and never a raw
//! struct cast.
//!
//! A byte-order slip is invisible to a round-trip test — the writer and
//! reader would agree with each other while disagreeing with the rest of
//! the world — so the defence is the cross-validation gate against a
//! real kernel, not self-consistency.
//!
//! # Layout
//!
//! The superblock lives at the start of every allocation group; AG 0's
//! copy at block 0 is authoritative and the rest are backups. The
//! on-disk structure is 264 bytes: a v4 prefix of 208 bytes followed by
//! the v5 extension (feature masks, CRC, metadata UUID, LSN).

use crate::error::{Error, Result};

/// `XFSB` — the superblock magic, big-endian at offset 0.
///
/// The bytes are `X`, `F`, `S`, `B` in that order on disk. Written as a
/// big-endian u32 that is 0x5846_5342 — note the ASCII, not an intuitive
/// grouping: an earlier transposition here parsed every hand-built test
/// fixture happily and rejected every real filesystem.
pub const XFS_SB_MAGIC: u32 = 0x5846_5342;

/// Size of the on-disk superblock structure in bytes.
pub const XFS_SB_SIZE: usize = 264;

/// Byte offset of `sb_crc` within the superblock. The CRC is computed
/// over the whole structure with these four bytes treated as zero.
pub(crate) const SB_CRC_OFFSET: usize = 224;

/// Mask selecting the version number from `sb_versionnum`.
const XFS_SB_VERSION_NUMBITS: u16 = 0x000f;

/// Feature bits carried in the top of `sb_versionnum` (v4-era features).
pub mod version_flags {
    /// Extended attributes are in use.
    pub const ATTRBIT: u16 = 0x0010;
    /// 64-bit inode numbers may be present.
    pub const NLINKBIT: u16 = 0x0020;
    /// Quota accounting is or has been enabled.
    pub const QUOTABIT: u16 = 0x0040;
    /// Inode alignment is in force (`sb_inoalignmt` is meaningful).
    pub const ALIGNBIT: u16 = 0x0080;
    /// Stripe alignment (`sb_unit` / `sb_width`) is in force.
    pub const DALIGNBIT: u16 = 0x0100;
    /// Logs may use a non-default stripe unit.
    pub const LOGV2BIT: u16 = 0x0400;
    /// Sector size may differ from 512 bytes.
    pub const SECTORBIT: u16 = 0x0800;
    /// `sb_features2` is valid.
    pub const MOREBITSBIT: u16 = 0x8000;
}

/// `sb_features_incompat` bits. A volume setting a bit this driver does
/// not understand cannot be read at all, let alone written.
pub mod incompat {
    /// Directory entries carry a file type byte.
    pub const FTYPE: u32 = 1 << 0;
    /// Sparse inode chunks are allowed.
    pub const SPINODES: u32 = 1 << 1;
    /// `sb_meta_uuid` holds the UUID stamped into metadata blocks.
    pub const META_UUID: u32 = 1 << 2;
    /// Large (64-bit) timestamps.
    pub const BIGTIME: u32 = 1 << 3;
    /// 64-bit per-inode extent counters.
    pub const NREXT64: u32 = 1 << 5;

    /// Every incompatible feature this driver can read.
    pub const SUPPORTED: u32 = FTYPE | SPINODES | META_UUID | BIGTIME | NREXT64;
}

/// `sb_features_ro_compat` bits. Unknown bits here still permit a
/// read-only mount, which is exactly what this driver does today.
pub mod ro_compat {
    /// Free inode B+tree (`finobt`) is present.
    pub const FINOBT: u32 = 1 << 0;
    /// Reverse-mapping B+tree (`rmapbt`) is present.
    pub const RMAPBT: u32 = 1 << 1;
    /// Reflinked (shared) file extents are allowed.
    pub const REFLINK: u32 = 1 << 2;
    /// Inode B+tree block counters are maintained.
    pub const INOBTCNT: u32 = 1 << 3;
}

/// A parsed XFS superblock.
///
/// Field names follow the on-disk `sb_*` names so a reader can match
/// this against the format documentation without a translation table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superblock {
    /// Filesystem block size in bytes.
    pub blocksize: u32,
    /// Total data-section blocks.
    pub dblocks: u64,
    /// Total real-time section blocks.
    pub rblocks: u64,
    /// Filesystem UUID.
    pub uuid: [u8; 16],
    /// First block of the internal log, or 0 when the log is external.
    pub logstart: u64,
    /// Root directory inode number.
    pub rootino: u64,
    /// Blocks per allocation group.
    pub agblocks: u32,
    /// Number of allocation groups.
    pub agcount: u32,
    /// Log length in filesystem blocks.
    pub logblocks: u32,
    /// Raw `sb_versionnum`, including the v4-era feature bits.
    pub versionnum: u16,
    /// Sector size in bytes.
    pub sectsize: u16,
    /// Inode size in bytes.
    pub inodesize: u16,
    /// Inodes per filesystem block.
    pub inopblock: u16,
    /// `log2(blocksize)`.
    pub blocklog: u8,
    /// `log2(sectsize)`.
    pub sectlog: u8,
    /// `log2(inodesize)`.
    pub inodelog: u8,
    /// `log2(inopblock)`.
    pub inopblog: u8,
    /// `log2(agblocks)`, rounded up. Used to split an inode number into
    /// its AG index and within-AG parts.
    pub agblklog: u8,
    /// Non-zero while `mkfs` is still writing the filesystem.
    pub inprogress: u8,
    /// Allocated inodes.
    pub icount: u64,
    /// Free inodes.
    pub ifree: u64,
    /// Free data blocks.
    pub fdblocks: u64,
    /// Inode alignment in blocks.
    pub inoalignmt: u32,
    /// `log2(directory block size / blocksize)`.
    pub dirblklog: u8,
    /// Log stripe unit in bytes.
    pub logsunit: u32,
    /// `sb_features2`.
    pub features2: u32,
    /// v5 compatible feature mask (0 on v4).
    pub features_compat: u32,
    /// v5 read-only-compatible feature mask (0 on v4).
    pub features_ro_compat: u32,
    /// v5 incompatible feature mask (0 on v4).
    pub features_incompat: u32,
    /// v5 log incompatible feature mask (0 on v4).
    pub features_log_incompat: u32,
    /// Sparse inode chunk alignment.
    pub spino_align: u32,
    /// UUID stamped into metadata block headers. Equals [`Self::uuid`]
    /// unless the `META_UUID` incompatible feature is set.
    pub meta_uuid: [u8; 16],
    /// Volume label, trailing NULs trimmed.
    pub fname: String,
}

/// Read a big-endian `u16` at `off`.
#[inline]
fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
}

/// Read a big-endian `u32` at `off`.
#[inline]
fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Read a big-endian `u64` at `off`.
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

/// Read a **little-endian** `u32` at `off`.
///
/// Used only for checksum fields. XFS is big-endian everywhere else, but
/// its CRCs are stored little-endian — the kernel's `xfs_end_cksum()`
/// returns `~cpu_to_le32(crc)`. Reading a CRC big-endian like the rest of
/// the structure makes every real filesystem look corrupt while
/// hand-built fixtures pass, because they would be written with the same
/// mistake.
#[inline]
pub(crate) fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl Superblock {
    /// Parse and validate a superblock from the first [`XFS_SB_SIZE`]
    /// bytes of `buf`.
    ///
    /// # Errors
    ///
    /// [`Error::NotXfs`] if the magic does not match,
    /// [`Error::BadSuperblock`] if a geometry field is out of range or
    /// inconsistent with its `log2` companion, [`Error::ChecksumMismatch`]
    /// if a v5 superblock fails its CRC, and
    /// [`Error::UnsupportedFeature`] if an incompatible feature bit this
    /// driver does not implement is set.
    pub fn parse(buf: &[u8]) -> Result<Self> {
        if buf.len() < XFS_SB_SIZE {
            return Err(Error::BadSuperblock(format!(
                "need {XFS_SB_SIZE} bytes, got {}",
                buf.len()
            )));
        }

        let magic = be32(buf, 0);
        if magic != XFS_SB_MAGIC {
            return Err(Error::NotXfs { magic });
        }

        let versionnum = be16(buf, 100);
        let version = versionnum & XFS_SB_VERSION_NUMBITS;
        if !(4..=5).contains(&version) {
            return Err(Error::BadSuperblock(format!(
                "unsupported version {version} (expected 4 or 5)"
            )));
        }
        let is_v5 = version == 5;

        // v5 superblocks are CRC32C protected. Verify before trusting any
        // other field: a bad CRC means the values below are meaningless,
        // and reporting "bad geometry" for a corrupt block misleads.
        //
        // The checksum covers the WHOLE SECTOR, not the 264-byte
        // structure -- XFS's verifier hands the full buffer length to
        // xfs_verify_cksum, so the trailing zero padding is included.
        // Checksumming only the struct passes on fixtures we build
        // ourselves and fails on every real filesystem.
        //
        // sb_sectsize is read before the checksum that protects it. That
        // is unavoidable (the length is needed to compute the sum) so it
        // is range-checked first; a wild value is rejected as a bad
        // superblock rather than used to index memory.
        let sectsize = be16(buf, 102);
        if !(512..=32768).contains(&sectsize) || !sectsize.is_power_of_two() {
            return Err(Error::BadSuperblock(format!(
                "sectsize {sectsize} is not a sane power of two"
            )));
        }
        if is_v5 {
            let end = usize::from(sectsize);
            if buf.len() < end {
                return Err(Error::BadSuperblock(format!(
                    "need a full {sectsize}-byte sector to verify the superblock checksum, got {}",
                    buf.len()
                )));
            }
            let stored = le32(buf, SB_CRC_OFFSET);
            let computed = crc32c_with_zeroed_crc(&buf[..end], SB_CRC_OFFSET);
            if stored != computed {
                return Err(Error::ChecksumMismatch {
                    what: "superblock",
                    block: 0,
                });
            }
        }

        let (features_compat, features_ro_compat, features_incompat, features_log_incompat) =
            if is_v5 {
                (
                    be32(buf, 208),
                    be32(buf, 212),
                    be32(buf, 216),
                    be32(buf, 220),
                )
            } else {
                (0, 0, 0, 0)
            };

        let unknown_incompat = features_incompat & !incompat::SUPPORTED;
        if unknown_incompat != 0 {
            return Err(Error::UnsupportedFeature(format!(
                "incompatible feature bits {unknown_incompat:#010x} not implemented"
            )));
        }

        let uuid: [u8; 16] = buf[32..48].try_into().expect("16 bytes");
        let meta_uuid: [u8; 16] = if is_v5 && features_incompat & incompat::META_UUID != 0 {
            buf[248..264].try_into().expect("16 bytes")
        } else {
            uuid
        };

        let fname_raw = &buf[108..120];
        let fname = String::from_utf8_lossy(fname_raw)
            .trim_end_matches('\0')
            .to_string();

        let sb = Superblock {
            blocksize: be32(buf, 4),
            dblocks: be64(buf, 8),
            rblocks: be64(buf, 16),
            uuid,
            logstart: be64(buf, 48),
            rootino: be64(buf, 56),
            agblocks: be32(buf, 84),
            agcount: be32(buf, 88),
            logblocks: be32(buf, 96),
            versionnum,
            sectsize: be16(buf, 102),
            inodesize: be16(buf, 104),
            inopblock: be16(buf, 106),
            blocklog: buf[120],
            sectlog: buf[121],
            inodelog: buf[122],
            inopblog: buf[123],
            agblklog: buf[124],
            inprogress: buf[126],
            icount: be64(buf, 128),
            ifree: be64(buf, 136),
            fdblocks: be64(buf, 144),
            inoalignmt: be32(buf, 180),
            dirblklog: buf[192],
            logsunit: be32(buf, 196),
            features2: be32(buf, 200),
            features_compat,
            features_ro_compat,
            features_incompat,
            features_log_incompat,
            spino_align: if is_v5 { be32(buf, 228) } else { 0 },
            meta_uuid,
            fname,
        };

        sb.validate()?;
        Ok(sb)
    }

    /// Structural sanity checks.
    ///
    /// Each `log2` field must agree with the value it describes. That
    /// redundancy is the cheapest available detector for reading the
    /// superblock at the wrong offset or with the wrong byte order: a
    /// little-endian misread of `blocksize` yields a value whose `log2`
    /// cannot match `blocklog`.
    fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::BadSuperblock(m));

        if !(512..=65536).contains(&self.blocksize) || !self.blocksize.is_power_of_two() {
            return bad(format!(
                "blocksize {} is not a sane power of two",
                self.blocksize
            ));
        }
        if 1u32 << self.blocklog != self.blocksize {
            return bad(format!(
                "blocklog {} does not describe blocksize {}",
                self.blocklog, self.blocksize
            ));
        }
        if !(512..=32768).contains(&self.sectsize) || !self.sectsize.is_power_of_two() {
            return bad(format!(
                "sectsize {} is not a sane power of two",
                self.sectsize
            ));
        }
        if 1u16 << self.sectlog != self.sectsize {
            return bad(format!(
                "sectlog {} does not describe sectsize {}",
                self.sectlog, self.sectsize
            ));
        }
        if !(256..=2048).contains(&self.inodesize) || !self.inodesize.is_power_of_two() {
            return bad(format!(
                "inodesize {} is not a sane power of two",
                self.inodesize
            ));
        }
        if 1u16 << self.inodelog != self.inodesize {
            return bad(format!(
                "inodelog {} does not describe inodesize {}",
                self.inodelog, self.inodesize
            ));
        }
        if self.inopblock as u32 != self.blocksize / self.inodesize as u32 {
            return bad(format!(
                "inopblock {} disagrees with blocksize {} / inodesize {}",
                self.inopblock, self.blocksize, self.inodesize
            ));
        }
        if self.agcount == 0 {
            return bad("agcount is zero".into());
        }
        if self.agblocks == 0 {
            return bad("agblocks is zero".into());
        }
        // agblklog must be large enough to hold an AG-relative block
        // number; it is log2(agblocks) rounded up.
        let need = 32 - (self.agblocks - 1).leading_zeros();
        if u32::from(self.agblklog) < need {
            return bad(format!(
                "agblklog {} too small for agblocks {}",
                self.agblklog, self.agblocks
            ));
        }
        // The AGs must be able to cover the data section.
        let covered = u64::from(self.agcount) * u64::from(self.agblocks);
        if covered < self.dblocks {
            return bad(format!(
                "agcount {} * agblocks {} = {covered} does not cover dblocks {}",
                self.agcount, self.agblocks, self.dblocks
            ));
        }
        if self.rootino == 0 {
            return bad("root inode number is zero".into());
        }
        if self.inprogress != 0 {
            return bad("sb_inprogress set — mkfs did not finish writing this volume".into());
        }
        Ok(())
    }

    /// On-disk format version: 4 or 5.
    pub fn version(&self) -> u16 {
        self.versionnum & XFS_SB_VERSION_NUMBITS
    }

    /// Whether this is a v5 (CRC-protected, self-describing metadata)
    /// filesystem. v5 is what every `mkfs.xfs` since 2014 produces by
    /// default, and it is the version this driver targets first.
    pub fn is_v5(&self) -> bool {
        self.version() == 5
    }

    /// Directory block size in bytes. Directories use a multiple of the
    /// filesystem block size, chosen at mkfs time.
    pub fn dirblocksize(&self) -> u32 {
        self.blocksize << self.dirblklog
    }

    /// Whether the log lives inside the data section. An external log
    /// device is not addressable through the block device we were given.
    pub fn has_internal_log(&self) -> bool {
        self.logstart != 0
    }

    /// Split an inode number into `(ag_index, ag_block, offset_in_block)`.
    ///
    /// XFS packs all three into one 64-bit inode number: the low
    /// `inopblog` bits index the inode within its block, the next
    /// `agblklog` bits give the AG-relative block, and the remainder is
    /// the allocation group index.
    pub fn split_ino(&self, ino: u64) -> (u32, u32, u32) {
        let off_mask = (1u64 << self.inopblog) - 1;
        let blk_mask = (1u64 << self.agblklog) - 1;
        let offset = (ino & off_mask) as u32;
        let ag_block = ((ino >> self.inopblog) & blk_mask) as u32;
        let ag = (ino >> (self.inopblog + self.agblklog)) as u32;
        (ag, ag_block, offset)
    }

    /// Whether the free inode B+tree is present.
    pub fn has_finobt(&self) -> bool {
        self.features_ro_compat & ro_compat::FINOBT != 0
    }

    /// Whether the reverse-mapping B+tree is present. When it is, every
    /// allocated extent has a reverse entry naming its owner — the
    /// strongest cross-check available to a consistency audit.
    pub fn has_rmapbt(&self) -> bool {
        self.features_ro_compat & ro_compat::RMAPBT != 0
    }

    /// Whether reflinked (shared) extents are allowed.
    pub fn has_reflink(&self) -> bool {
        self.features_ro_compat & ro_compat::REFLINK != 0
    }

    /// Whether directory entries carry a file-type byte.
    pub fn has_ftype(&self) -> bool {
        self.features_incompat & incompat::FTYPE != 0
    }
}

/// CRC32C over `buf` with the four checksum bytes at `crc_off` treated
/// as zero — the convention every self-describing XFS metadata block
/// uses, since the stored CRC cannot cover itself.
pub(crate) fn crc32c_with_zeroed_crc(buf: &[u8], crc_off: usize) -> u32 {
    let mut c = crc32c::crc32c(&buf[..crc_off]);
    c = crc32c::crc32c_append(c, &[0, 0, 0, 0]);
    crc32c::crc32c_append(c, &buf[crc_off + 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syntactically valid v4 superblock for tests: 4 KiB blocks,
    /// 512-byte sectors, 512-byte inodes, 4 AGs.
    fn v4_superblock() -> Vec<u8> {
        // A full 512-byte sector, not just the 264-byte struct: the v5
        // checksum covers the whole sector.
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
        b[8..16].copy_from_slice(&4000u64.to_be_bytes()); // dblocks
        b[48..56].copy_from_slice(&100u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&1000u32.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
        b[100..102].copy_from_slice(&4u16.to_be_bytes()); // versionnum
        b[102..104].copy_from_slice(&512u16.to_be_bytes()); // sectsize
        b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        b[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
        b[120] = 12; // blocklog
        b[121] = 9; // sectlog
        b[122] = 9; // inodelog
        b[123] = 3; // inopblog
        b[124] = 10; // agblklog
        b
    }

    /// Promote a v4 test superblock to v5 and fix up its CRC.
    fn v5_superblock() -> Vec<u8> {
        let mut b = v4_superblock();
        b[100..102].copy_from_slice(&5u16.to_be_bytes());
        let crc = crc32c_with_zeroed_crc(&b, SB_CRC_OFFSET);
        b[SB_CRC_OFFSET..SB_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        b
    }

    #[test]
    fn parses_v4() {
        let sb = Superblock::parse(&v4_superblock()).unwrap();
        assert_eq!(sb.version(), 4);
        assert!(!sb.is_v5());
        assert_eq!(sb.blocksize, 4096);
        assert_eq!(sb.agcount, 4);
        assert_eq!(sb.rootino, 128);
    }

    #[test]
    fn parses_v5_and_checks_crc() {
        let sb = Superblock::parse(&v5_superblock()).unwrap();
        assert!(sb.is_v5());
        assert_eq!(sb.blocksize, 4096);
    }

    #[test]
    fn rejects_bad_v5_crc() {
        let mut b = v5_superblock();
        b[SB_CRC_OFFSET] ^= 0xFF;
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_wrong_magic() {
        let mut b = v4_superblock();
        b[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_be_bytes());
        assert!(matches!(Superblock::parse(&b), Err(Error::NotXfs { .. })));
    }

    /// The decisive byte-order regression test. A little-endian reader
    /// sees the magic as 0x53465842 and must reject the volume outright.
    #[test]
    fn little_endian_magic_is_not_accepted() {
        let mut b = v4_superblock();
        b[0..4].copy_from_slice(&XFS_SB_MAGIC.to_le_bytes());
        assert!(matches!(Superblock::parse(&b), Err(Error::NotXfs { .. })));
    }

    /// The log2 fields exist to catch exactly this: a geometry value read
    /// at the wrong offset or in the wrong byte order.
    #[test]
    fn rejects_blocklog_disagreeing_with_blocksize() {
        let mut b = v4_superblock();
        b[120] = 11; // claims 2 KiB blocks while blocksize says 4 KiB
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_inopblock_disagreeing_with_geometry() {
        let mut b = v4_superblock();
        b[106..108].copy_from_slice(&16u16.to_be_bytes()); // truth is 8
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_ags_not_covering_data_section() {
        let mut b = v4_superblock();
        b[8..16].copy_from_slice(&999_999u64.to_be_bytes()); // dblocks >> agcount*agblocks
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_mkfs_in_progress() {
        let mut b = v4_superblock();
        b[126] = 1;
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_unknown_incompat_feature() {
        let mut b = v4_superblock();
        b[100..102].copy_from_slice(&5u16.to_be_bytes());
        b[216..220].copy_from_slice(&(1u32 << 20).to_be_bytes()); // undefined bit
        let crc = crc32c_with_zeroed_crc(&b, SB_CRC_OFFSET);
        b[SB_CRC_OFFSET..SB_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Superblock::parse(&b),
            Err(Error::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(matches!(
            Superblock::parse(&[0u8; 64]),
            Err(Error::BadSuperblock(_))
        ));
    }

    /// An inode number packs (ag, block, offset) into one 64-bit value.
    #[test]
    fn splits_inode_number() {
        let sb = Superblock::parse(&v4_superblock()).unwrap();
        // inopblog = 3, agblklog = 10.
        // ag = 2, ag_block = 5, offset = 6
        //   -> (2 << 13) | (5 << 3) | 6
        let ino = (2u64 << 13) | (5u64 << 3) | 6;
        assert_eq!(sb.split_ino(ino), (2, 5, 6));
    }

    #[test]
    fn root_inode_splits_to_ag_zero() {
        let sb = Superblock::parse(&v4_superblock()).unwrap();
        let (ag, _, _) = sb.split_ino(sb.rootino);
        assert_eq!(ag, 0, "root inode always lives in AG 0");
    }

    #[test]
    fn meta_uuid_defaults_to_fs_uuid() {
        let sb = Superblock::parse(&v5_superblock()).unwrap();
        assert_eq!(sb.meta_uuid, sb.uuid);
    }
}
