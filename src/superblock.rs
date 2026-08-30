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

use crate::endian::{be16, be32, be64, le32, uuid_at};
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
pub(crate) const SB_CRC_OFFSET: usize = offsets::CRC;

/// Byte offsets of each field within the on-disk superblock.
///
/// Named rather than inlined at the call site for the same reason the
/// sibling Btrfs driver names its own: a numeric literal in a parse
/// expression carries no way to tell a correct offset from a typo, and
/// two of the three bugs this crate has shipped were exactly that — a
/// value read from the wrong place or with the wrong span. A name can be
/// checked against the format documentation by eye; `56` cannot.
pub mod offsets {
    /// `sb_magicnum` — `XFSB`.
    pub const MAGIC: usize = 0;
    /// `sb_blocksize` — filesystem block size in bytes.
    pub const BLOCKSIZE: usize = 4;
    /// `sb_dblocks` — total data-section blocks.
    pub const DBLOCKS: usize = 8;
    /// `sb_rblocks` — total real-time section blocks.
    pub const RBLOCKS: usize = 16;
    /// `sb_rextents` — real-time extents in the realtime section.
    pub const REXTENTS: usize = 24;
    /// `sb_uuid` — filesystem UUID.
    pub const UUID: usize = 32;
    /// `sb_logstart` — first block of the internal log, 0 if external.
    pub const LOGSTART: usize = 48;
    /// `sb_rootino` — root directory inode number.
    pub const ROOTINO: usize = 56;
    /// `sb_rbmino` — inode holding the realtime bitmap.
    pub const RBMINO: usize = 64;
    /// `sb_rsumino` — inode holding the realtime summary.
    pub const RSUMINO: usize = 72;
    /// `sb_rextsize` — realtime extent size in filesystem blocks.
    pub const REXTSIZE: usize = 80;
    /// `sb_agblocks` — blocks per allocation group.
    pub const AGBLOCKS: usize = 84;
    /// `sb_agcount` — number of allocation groups.
    pub const AGCOUNT: usize = 88;
    /// `sb_rbmblocks` — blocks the realtime bitmap occupies.
    pub const RBMBLOCKS: usize = 92;
    /// `sb_logblocks` — log length in filesystem blocks.
    pub const LOGBLOCKS: usize = 96;
    /// `sb_versionnum` — version plus the v4-era feature bits.
    pub const VERSIONNUM: usize = 100;
    /// `sb_sectsize` — sector size in bytes.
    pub const SECTSIZE: usize = 102;
    /// `sb_inodesize` — inode size in bytes.
    pub const INODESIZE: usize = 104;
    /// `sb_inopblock` — inodes per filesystem block.
    pub const INOPBLOCK: usize = 106;
    /// `sb_fname` — volume label.
    pub const FNAME: usize = 108;
    /// Length of `sb_fname`.
    pub const FNAME_LEN: usize = 12;
    /// `sb_blocklog` — log2 of the block size.
    pub const BLOCKLOG: usize = 120;
    /// `sb_sectlog` — log2 of the sector size.
    pub const SECTLOG: usize = 121;
    /// `sb_inodelog` — log2 of the inode size.
    pub const INODELOG: usize = 122;
    /// `sb_inopblog` — log2 of inodes per block.
    pub const INOPBLOG: usize = 123;
    /// `sb_agblklog` — log2 of blocks per AG, rounded up.
    pub const AGBLKLOG: usize = 124;
    /// `sb_rextslog` — log2 of the realtime extent count.
    pub const REXTSLOG: usize = 125;
    /// `sb_inprogress` — non-zero while mkfs is still writing.
    pub const INPROGRESS: usize = 126;
    /// `sb_imax_pct` — percentage of the filesystem inodes may occupy.
    pub const IMAX_PCT: usize = 127;
    /// `sb_icount` — allocated inodes.
    pub const ICOUNT: usize = 128;
    /// `sb_ifree` — free inodes.
    pub const IFREE: usize = 136;
    /// `sb_fdblocks` — free data blocks.
    pub const FDBLOCKS: usize = 144;
    /// `sb_frextents` — free realtime extents.
    pub const FREXTENTS: usize = 152;
    /// `sb_uquotino` — user quota inode, 0 when unset.
    pub const UQUOTINO: usize = 160;
    /// `sb_gquotino` — group quota inode, 0 when unset.
    pub const GQUOTINO: usize = 168;
    /// `sb_qflags` — quota accounting and enforcement flags.
    pub const QFLAGS: usize = 176;
    /// `sb_flags` — miscellaneous filesystem flags.
    pub const FLAGS: usize = 178;
    /// `sb_shared_vn` — shared version number; 0 on every modern volume.
    pub const SHARED_VN: usize = 179;
    /// `sb_inoalignmt` — inode alignment in blocks.
    pub const INOALIGNMT: usize = 180;
    /// `sb_unit` — RAID stripe unit in filesystem blocks.
    pub const UNIT: usize = 184;
    /// `sb_width` — RAID stripe width in filesystem blocks.
    pub const WIDTH: usize = 188;
    /// `sb_dirblklog` — log2 of directory block size over block size.
    pub const DIRBLKLOG: usize = 192;
    /// `sb_logsectlog` — log2 of the log's sector size.
    pub const LOGSECTLOG: usize = 193;
    /// `sb_logsectsize` — the log's sector size in bytes.
    pub const LOGSECTSIZE: usize = 194;
    /// `sb_logsunit` — log stripe unit in bytes.
    pub const LOGSUNIT: usize = 196;
    /// `sb_features2`.
    pub const FEATURES2: usize = 200;
    /// `sb_bad_features2` — the mirror of `sb_features2` written by
    /// kernels with the alignment bug. Carried, never trusted.
    pub const BAD_FEATURES2: usize = 204;
    /// `sb_features_compat` — v5 only.
    pub const FEATURES_COMPAT: usize = 208;
    /// `sb_features_ro_compat` — v5 only.
    pub const FEATURES_RO_COMPAT: usize = 212;
    /// `sb_features_incompat` — v5 only.
    pub const FEATURES_INCOMPAT: usize = 216;
    /// `sb_features_log_incompat` — v5 only.
    pub const FEATURES_LOG_INCOMPAT: usize = 220;
    /// `sb_crc` — CRC32C, stored little-endian. v5 only.
    pub const CRC: usize = 224;
    /// `sb_spino_align` — sparse inode chunk alignment. v5 only.
    pub const SPINO_ALIGN: usize = 228;
    /// `sb_pquotino` — project quota inode. v5 only.
    pub const PQUOTINO: usize = 232;
    /// `sb_lsn` — log sequence number of the last superblock write.
    /// v5 only.
    pub const LSN: usize = 240;
    /// `sb_meta_uuid` — UUID stamped into metadata blocks. v5 only.
    pub const META_UUID: usize = 248;
}

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

/// `sb_features2` bits. Only meaningful when `MOREBITSBIT` is set in
/// `sb_versionnum`.
pub mod features2_flags {
    /// Directory entries carry a file type byte. This is how a v4
    /// filesystem advertises the feature; v5 uses the incompatible mask.
    pub const FTYPE: u32 = 0x0000_0200;
    /// Lazy superblock counters.
    pub const LAZYSBCOUNT: u32 = 0x0000_0002;
    /// Extended attributes version 2.
    pub const ATTR2: u32 = 0x0000_0008;
    /// 32-bit project identifiers.
    pub const PROJID32BIT: u32 = 0x0000_0080;
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

    // ---------------------------------------------------------------
    // Fields below this line are modelled so a superblock can be
    // WRITTEN, not because reading needs them. A formatter has to emit
    // every field; a reader of a data-section-only filesystem needs
    // almost none of them. They are parsed and round-tripped exactly,
    // and nothing in this crate reads them to make a decision.
    //
    // Splitting them out into their own struct was the alternative.
    // Kept flat so the field order still matches the on-disk order,
    // which is the property that makes an offset typo visible by eye.
    // ---------------------------------------------------------------
    /// `sb_rextents` — realtime extents in the realtime section.
    pub rextents: u64,
    /// `sb_rbmino` — inode holding the realtime bitmap.
    pub rbmino: u64,
    /// `sb_rsumino` — inode holding the realtime summary.
    pub rsumino: u64,
    /// `sb_rextsize` — realtime extent size in filesystem blocks.
    pub rextsize: u32,
    /// `sb_rbmblocks` — blocks the realtime bitmap occupies.
    pub rbmblocks: u32,
    /// `sb_rextslog` — log2 of the realtime extent count.
    pub rextslog: u8,
    /// `sb_imax_pct` — percentage of the filesystem inodes may occupy.
    pub imax_pct: u8,
    /// `sb_frextents` — free realtime extents.
    pub frextents: u64,
    /// `sb_uquotino` — user quota inode, 0 when unset.
    pub uquotino: u64,
    /// `sb_gquotino` — group quota inode, 0 when unset.
    pub gquotino: u64,
    /// `sb_qflags` — quota accounting and enforcement flags.
    pub qflags: u16,
    /// `sb_flags` — miscellaneous filesystem flags.
    pub flags: u8,
    /// `sb_shared_vn` — shared version number; 0 on every modern volume.
    pub shared_vn: u8,
    /// `sb_unit` — RAID stripe unit in filesystem blocks.
    pub unit: u32,
    /// `sb_width` — RAID stripe width in filesystem blocks.
    pub width: u32,
    /// `sb_logsectlog` — log2 of the log's sector size.
    pub logsectlog: u8,
    /// `sb_logsectsize` — the log's sector size in bytes.
    pub logsectsize: u16,
    /// `sb_bad_features2` — the mirror of [`Self::features2`] that
    /// kernels with the alignment bug wrote.
    ///
    /// It is carried so a superblock can be reproduced, and is never
    /// consulted: a filesystem where the two disagree is one the kernel
    /// itself repairs on mount, and guessing which side is right here
    /// would be a decision made with less information than the kernel
    /// has.
    pub bad_features2: u32,
    /// `sb_pquotino` — project quota inode. v5 only; 0 on v4.
    pub pquotino: u64,
    /// `sb_lsn` — log sequence number of the last superblock write.
    /// v5 only; 0 on v4.
    pub lsn: i64,
}

/// Read a **little-endian** `u32` at `off`.
///
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

        let magic = be32(buf, offsets::MAGIC);
        if magic != XFS_SB_MAGIC {
            return Err(Error::NotXfs { magic });
        }

        let versionnum = be16(buf, offsets::VERSIONNUM);
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
        let sectsize = be16(buf, offsets::SECTSIZE);
        if !(512..=32768).contains(&sectsize) || !sectsize.is_power_of_two() {
            return Err(Error::BadSuperblock(format!(
                "sectsize {sectsize} is not a sane power of two"
            )));
        }
        if is_v5 {
            verify_checksum(buf, sectsize)?;
        }

        let (features_compat, features_ro_compat, features_incompat, features_log_incompat) =
            if is_v5 {
                (
                    be32(buf, offsets::FEATURES_COMPAT),
                    be32(buf, offsets::FEATURES_RO_COMPAT),
                    be32(buf, offsets::FEATURES_INCOMPAT),
                    be32(buf, offsets::FEATURES_LOG_INCOMPAT),
                )
            } else {
                (0, 0, 0, 0)
            };

        reject_unsupported_features(features_incompat)?;

        let uuid = uuid_at(buf, offsets::UUID);
        let meta_uuid: [u8; 16] = if is_v5 && features_incompat & incompat::META_UUID != 0 {
            uuid_at(buf, offsets::META_UUID)
        } else {
            uuid
        };

        let fname_raw = &buf[offsets::FNAME..offsets::FNAME + offsets::FNAME_LEN];
        let fname = String::from_utf8_lossy(fname_raw)
            .trim_end_matches('\0')
            .to_string();

        let sb = Superblock {
            blocksize: be32(buf, offsets::BLOCKSIZE),
            dblocks: be64(buf, offsets::DBLOCKS),
            rblocks: be64(buf, offsets::RBLOCKS),
            uuid,
            logstart: be64(buf, offsets::LOGSTART),
            rootino: be64(buf, offsets::ROOTINO),
            agblocks: be32(buf, offsets::AGBLOCKS),
            agcount: be32(buf, offsets::AGCOUNT),
            logblocks: be32(buf, offsets::LOGBLOCKS),
            versionnum,
            sectsize: be16(buf, offsets::SECTSIZE),
            inodesize: be16(buf, offsets::INODESIZE),
            inopblock: be16(buf, offsets::INOPBLOCK),
            blocklog: buf[offsets::BLOCKLOG],
            sectlog: buf[offsets::SECTLOG],
            inodelog: buf[offsets::INODELOG],
            inopblog: buf[offsets::INOPBLOG],
            agblklog: buf[offsets::AGBLKLOG],
            inprogress: buf[offsets::INPROGRESS],
            icount: be64(buf, offsets::ICOUNT),
            ifree: be64(buf, offsets::IFREE),
            fdblocks: be64(buf, offsets::FDBLOCKS),
            inoalignmt: be32(buf, offsets::INOALIGNMT),
            dirblklog: buf[offsets::DIRBLKLOG],
            logsunit: be32(buf, offsets::LOGSUNIT),
            features2: be32(buf, offsets::FEATURES2),
            features_compat,
            features_ro_compat,
            features_incompat,
            features_log_incompat,
            spino_align: if is_v5 {
                be32(buf, offsets::SPINO_ALIGN)
            } else {
                0
            },
            meta_uuid,
            fname,

            rextents: be64(buf, offsets::REXTENTS),
            rbmino: be64(buf, offsets::RBMINO),
            rsumino: be64(buf, offsets::RSUMINO),
            rextsize: be32(buf, offsets::REXTSIZE),
            rbmblocks: be32(buf, offsets::RBMBLOCKS),
            rextslog: buf[offsets::REXTSLOG],
            imax_pct: buf[offsets::IMAX_PCT],
            frextents: be64(buf, offsets::FREXTENTS),
            uquotino: be64(buf, offsets::UQUOTINO),
            gquotino: be64(buf, offsets::GQUOTINO),
            qflags: be16(buf, offsets::QFLAGS),
            flags: buf[offsets::FLAGS],
            shared_vn: buf[offsets::SHARED_VN],
            unit: be32(buf, offsets::UNIT),
            width: be32(buf, offsets::WIDTH),
            logsectlog: buf[offsets::LOGSECTLOG],
            logsectsize: be16(buf, offsets::LOGSECTSIZE),
            bad_features2: be32(buf, offsets::BAD_FEATURES2),
            // Both live past the 208-byte v4 structure. On a v4
            // filesystem those bytes are not `sb_pquotino` and
            // `sb_lsn`; they are whatever follows the superblock, so
            // reading them would report a field that does not exist.
            pquotino: if is_v5 {
                be64(buf, offsets::PQUOTINO)
            } else {
                0
            },
            lsn: if is_v5 {
                be64(buf, offsets::LSN) as i64
            } else {
                0
            },
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
    /// Bytes in one inode cluster — the unit inodes are read and
    /// written in, and the buffer a logged inode addresses.
    ///
    /// A logged inode does not name its own address. It names the
    /// cluster buffer holding it and its offset inside that buffer, so
    /// a writer that gets this wrong produces a record the kernel
    /// accepts, starts replaying, and then fails on with an I/O error
    /// against block zero.
    ///
    /// # The rule
    ///
    /// The base is 8 KiB. On v5, where inodes carry the v3 core, it is
    /// scaled by how many 256-byte minimum inodes fit in one of this
    /// filesystem's — so a 512-byte inode doubles it and a 2 KiB inode
    /// takes it to 64 KiB. The result is then truncated to a whole
    /// number of filesystem blocks, since a cluster is read as blocks.
    ///
    /// Measured, not assumed: 7,260 inode items the kernel wrote across
    /// four allocation groups and four geometries, every one of them
    /// naming a buffer of exactly this size, aligned to it, with the
    /// inode's offset inside it accounted for to the byte.
    ///
    /// Untested at a 64 KiB block size, where the truncation would take
    /// a smaller cluster to zero blocks and the clamp below applies —
    /// mounting such a filesystem needs a kernel with matching pages,
    /// which the oracle does not have.
    pub fn inode_cluster_bytes(&self) -> u32 {
        /// `XFS_INODE_BIG_CLUSTER_SIZE`.
        const BASE: u32 = 8192;
        /// `XFS_DINODE_MIN_SIZE` — the smallest inode the format allows,
        /// and so the unit the scaling above is expressed in.
        const MIN_INODE: u32 = 256;

        let raw = if self.is_v5() {
            BASE * (u32::from(self.inodesize) / MIN_INODE)
        } else {
            BASE
        };
        let blocks = (raw / self.blocksize).max(1);
        blocks * self.blocksize
    }

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

    /// Rebuild an inode number from its allocation group and its
    /// group-relative number.
    ///
    /// The inverse of [`Superblock::split_ino`] for the case where the
    /// group-relative part is already combined — which is the form the
    /// inode B+trees store, since a chunk's `ir_startino` is relative to
    /// the group rather than to the filesystem.
    pub fn join_ino(&self, ag: u32, agino: u32) -> u64 {
        (u64::from(ag) << (self.inopblog + self.agblklog)) | u64::from(agino)
    }

    /// Split a filesystem block number into `(ag_index, ag_block)`.
    ///
    /// An XFS block number is not a linear index into the device. Like an
    /// inode number it is packed: the low `agblklog` bits give the block
    /// within its allocation group, the remainder the group itself.
    /// Treating one as linear reads from the wrong place on any
    /// filesystem with more than one allocation group — which is every
    /// filesystem of consequential size.
    pub fn split_fsblock(&self, fsblock: u64) -> (u32, u32) {
        let mask = (1u64 << self.agblklog) - 1;
        let ag = (fsblock >> self.agblklog) as u32;
        let ag_block = (fsblock & mask) as u32;
        (ag, ag_block)
    }

    /// Byte offset of a filesystem block within the device.
    pub fn fsblock_offset(&self, fsblock: u64) -> u64 {
        let (ag, ag_block) = self.split_fsblock(fsblock);
        (u64::from(ag) * u64::from(self.agblocks) + u64::from(ag_block)) * u64::from(self.blocksize)
    }

    /// Whether inode chunks may be sparse — that is, whether a chunk of
    /// 64 inodes may have some of its blocks missing.
    ///
    /// This decides how an inode B+tree record is read, and it is the
    /// **feature** that decides rather than the format version. A v5
    /// filesystem made with `-i sparse=0` writes the same plain 32-bit
    /// free count a v4 one does; only with this bit set are those four
    /// bytes a hole mask, a chunk count and an 8-bit free count. See
    /// [`crate::inode_btree`].
    pub fn has_sparse_inodes(&self) -> bool {
        self.features_incompat & incompat::SPINODES != 0
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
    ///
    /// Two independent conditions, because the feature predates v5. A v5
    /// filesystem advertises it through the incompatible feature mask; a
    /// v4 one advertises it in `sb_features2`, which is only meaningful
    /// when `MOREBITSBIT` is set in the version number.
    ///
    /// Checking only the v5 bit reads every entry on a v4 filesystem one
    /// byte short, which shifts the inode number that follows the name
    /// and corrupts the whole listing.
    pub fn has_ftype(&self) -> bool {
        if self.features_incompat & incompat::FTYPE != 0 {
            return true;
        }
        self.versionnum & version_flags::MOREBITSBIT != 0
            && self.features2 & features2_flags::FTYPE != 0
    }
}

/// Verify the v5 superblock checksum.
///
/// The sum covers the whole sector, not the 264-byte structure: XFS
/// hands the full buffer length to its verifier, so the trailing zero
/// padding is included. Checksumming only the struct passes on fixtures
/// built by this crate and fails on every real filesystem — a bug this
/// driver has already shipped once.
fn verify_checksum(buf: &[u8], sectsize: u16) -> Result<()> {
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
    Ok(())
}

/// Refuse a volume that sets an incompatible feature bit this driver
/// does not implement.
///
/// Incompatible means exactly that: the on-disk layout differs in a way
/// that makes reading unsafe, so guessing is worse than declining.
/// Read-only-compatible bits are deliberately not checked here — an
/// unknown one still permits the read-only mount this driver performs.
fn reject_unsupported_features(features_incompat: u32) -> Result<()> {
    let unknown = features_incompat & !incompat::SUPPORTED;
    if unknown != 0 {
        return Err(Error::UnsupportedFeature(format!(
            "incompatible feature bits {unknown:#010x} not implemented"
        )));
    }
    Ok(())
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

    /// Rebuild a v5 superblock with a different inode size, so the rule
    /// that scales the cluster can be exercised at more than one point.
    fn v5_with_inodesize(inodesize: u16, blocksize: u32) -> Superblock {
        let mut b = v4_superblock();
        b[100..102].copy_from_slice(&5u16.to_be_bytes());
        b[4..8].copy_from_slice(&blocksize.to_be_bytes());
        b[104..106].copy_from_slice(&inodesize.to_be_bytes());
        b[106..108].copy_from_slice(&((blocksize / u32::from(inodesize)) as u16).to_be_bytes());
        b[120] = blocksize.trailing_zeros() as u8;
        b[122] = inodesize.trailing_zeros() as u8;
        b[123] = (blocksize / u32::from(inodesize)).trailing_zeros() as u8;
        let crc = crc32c_with_zeroed_crc(&b, SB_CRC_OFFSET);
        b[SB_CRC_OFFSET..SB_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).expect("superblock")
    }

    /// The cluster is 8 KiB scaled by how many minimum-size inodes fit
    /// in one of this filesystem's, then truncated to whole blocks.
    ///
    /// The three v5 cases are the ones the oracle confirms against
    /// filesystems the kernel wrote; they are repeated here so the rule
    /// is still covered where there are no fixtures to read.
    #[test]
    fn the_inode_cluster_scales_with_the_inode_size() {
        for (blocksize, inodesize, expect) in [
            (4096u32, 512u16, 16384u32),
            (1024, 512, 16384),
            (4096, 1024, 32768),
            (4096, 2048, 65536),
        ] {
            let sb = v5_with_inodesize(inodesize, blocksize);
            assert_eq!(
                sb.inode_cluster_bytes(),
                expect,
                "blocksize {blocksize}, inodesize {inodesize}"
            );
        }
    }

    /// v4 does not scale: its inodes carry the older core, and the
    /// cluster stays at the 8 KiB base.
    #[test]
    fn a_v4_inode_cluster_does_not_scale() {
        let sb = Superblock::parse(&v4_superblock()).unwrap();
        assert!(!sb.is_v5());
        assert_eq!(sb.inode_cluster_bytes(), 8192);
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
