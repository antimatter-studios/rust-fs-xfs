//! On-disk inode (`xfs_dinode`) parsing.
//!
//! # Layout
//!
//! An inode is a fixed-size record — `sb_inodesize`, 512 bytes by
//! default — beginning with a core of 176 bytes on v3 (96 bytes of v2
//! core, `di_next_unlinked`, then the v3 extension). Everything after
//! the core is fork data: the data fork immediately follows, and when
//! `di_forkoff` is non-zero an attribute fork begins at that offset.
//!
//! # Byte order
//!
//! Big-endian throughout, as everywhere in XFS — with the standing
//! exception that `di_crc` is little-endian. See [`crate::superblock`].
//!
//! # Self-describing fields
//!
//! A v3 inode records its own inode number (`di_ino`) and the filesystem
//! UUID. Both are checked here against what the caller asked for, so an
//! inode read from the wrong offset is rejected rather than returned as
//! plausible-looking garbage.

use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, le32, Superblock};

/// `IN` — inode magic.
pub const XFS_DINODE_MAGIC: u16 = 0x494e;

/// Size of the v3 inode core, in bytes. Fork data begins here.
pub const XFS_DINODE_V3_SIZE: usize = 176;

/// Size of the v1/v2 inode core, in bytes.
pub const XFS_DINODE_V2_SIZE: usize = 100;

/// Byte offset of `di_crc` within the inode.
const DI_CRC_OFFSET: usize = 100;

/// How the data in a fork is stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Device number, for character and block devices.
    Dev,
    /// Data lives inline in the fork itself — short-form directories,
    /// short symlink targets.
    Local,
    /// An array of extent records.
    Extents,
    /// A B+tree of extent records, for files too fragmented to inline.
    Btree,
    /// Unused in modern XFS.
    Uuid,
    /// Reverse-mapping fork.
    Rmap,
}

impl Format {
    fn from_raw(v: u8) -> Result<Self> {
        Ok(match v {
            0 => Format::Dev,
            1 => Format::Local,
            2 => Format::Extents,
            3 => Format::Btree,
            4 => Format::Uuid,
            5 => Format::Rmap,
            other => {
                return Err(Error::BadSuperblock(format!(
                    "inode fork format {other} is not a defined value"
                )))
            }
        })
    }
}

/// File type, decoded from the mode's format bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    /// Regular file.
    Regular,
    /// Directory.
    Directory,
    /// Symbolic link.
    Symlink,
    /// Character device.
    CharDevice,
    /// Block device.
    BlockDevice,
    /// FIFO.
    Fifo,
    /// Unix domain socket.
    Socket,
}

/// A timestamp. XFS stores these either as a seconds/nanoseconds pair or,
/// with the `bigtime` feature, as a single 64-bit nanosecond count from a
/// different epoch — so the raw value is kept alongside the decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timestamp {
    /// Seconds since the Unix epoch. Negative values predate 1970.
    pub sec: i64,
    /// Nanoseconds within the second.
    pub nsec: u32,
}

/// The `bigtime` epoch is 1901-12-13 20:45:52 UTC, chosen so the 64-bit
/// nanosecond counter starts where the old signed 32-bit second counter
/// would have underflowed.
const BIGTIME_EPOCH_OFFSET_SECS: i64 = -2_147_483_648;

const NSEC_PER_SEC: u64 = 1_000_000_000;

impl Timestamp {
    /// Decode a timestamp from `buf` at `off`.
    ///
    /// `bigtime` selects the representation: a 64-bit nanosecond counter
    /// from the 1901 epoch, or the legacy pair of 32-bit seconds and
    /// nanoseconds.
    fn parse(buf: &[u8], off: usize, bigtime: bool) -> Self {
        if bigtime {
            let ns = be64(buf, off);
            Timestamp {
                sec: BIGTIME_EPOCH_OFFSET_SECS + (ns / NSEC_PER_SEC) as i64,
                nsec: (ns % NSEC_PER_SEC) as u32,
            }
        } else {
            Timestamp {
                sec: i64::from(be32(buf, off) as i32),
                nsec: be32(buf, off + 4),
            }
        }
    }
}

/// Inode flags (`di_flags`) this driver acts on.
pub mod flags {
    /// The inode's extents are on the real-time device.
    pub const REALTIME: u16 = 1 << 0;
    /// Extent size hint is set.
    pub const EXTSIZE: u16 = 1 << 3;
    /// Directory entries are hashed rather than sorted.
    pub const NOSYMLINKS: u16 = 1 << 10;
}

/// `di_flags2` bits.
pub mod flags2 {
    /// Extents may be shared with another inode (reflink).
    pub const REFLINK: u64 = 1 << 1;
    /// Timestamps use the 64-bit `bigtime` encoding.
    pub const BIGTIME: u64 = 1 << 3;
}

#[inline]
fn be16(b: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([b[off], b[off + 1]])
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

/// A parsed inode core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inode {
    /// Inode number.
    pub ino: u64,
    /// Raw mode, including permission and type bits.
    pub mode: u16,
    /// On-disk inode version: 1, 2 or 3.
    pub version: u8,
    /// How the data fork stores its contents.
    pub format: Format,
    /// How the attribute fork stores its contents.
    pub aformat: Format,
    /// Owning user id.
    pub uid: u32,
    /// Owning group id.
    pub gid: u32,
    /// Hard link count.
    pub nlink: u32,
    /// File size in bytes. For a directory, the size of its data fork.
    pub size: u64,
    /// Blocks allocated to this inode, data and attribute forks together.
    pub nblocks: u64,
    /// Number of extents in the data fork.
    pub nextents: u64,
    /// Number of extents in the attribute fork.
    pub anextents: u32,
    /// Attribute fork offset, in units of 8 bytes from the end of the
    /// inode core. Zero means there is no attribute fork.
    pub forkoff: u8,
    /// `di_flags`.
    pub flags: u16,
    /// `di_flags2` (v3 only).
    pub flags2: u64,
    /// Generation number, bumped on reuse.
    pub gen: u32,
    /// Next inode in the AGI unlinked list, or `u32::MAX` for none.
    pub next_unlinked: u32,
    /// Last access time.
    pub atime: Timestamp,
    /// Last modification time.
    pub mtime: Timestamp,
    /// Last inode-change time.
    pub ctime: Timestamp,
    /// Creation time (v3 only).
    pub crtime: Timestamp,
}

impl Inode {
    /// Parse the inode expected to have number `ino` from `buf`, which
    /// must hold at least `sb.inodesize` bytes starting at the inode.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] for a bad magic, an undefined fork
    /// format, or an internally inconsistent core;
    /// [`Error::ChecksumMismatch`] if a v3 inode fails its CRC; and
    /// [`Error::BlockIdentityMismatch`] if a v3 inode records a
    /// different inode number or filesystem UUID than expected.
    pub fn parse(buf: &[u8], sb: &Superblock, ino: u64) -> Result<Self> {
        let isize = usize::from(sb.inodesize);
        if buf.len() < isize {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: need {isize} bytes, got {}",
                buf.len()
            )));
        }

        let magic = be16(buf, 0);
        if magic != XFS_DINODE_MAGIC {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: magic {magic:#06x}, expected {XFS_DINODE_MAGIC:#06x}"
            )));
        }

        let version = buf[4];
        if !(1..=3).contains(&version) {
            return Err(Error::BadSuperblock(format!(
                "inode {ino}: version {version} is not 1, 2 or 3"
            )));
        }
        let is_v3 = version == 3;

        // v3 inodes are CRC32C protected over the whole inode record.
        // Verify before trusting any other field, as with the superblock.
        if is_v3 {
            let stored = le32(buf, DI_CRC_OFFSET);
            let computed = crc32c_with_zeroed_crc(&buf[..isize], DI_CRC_OFFSET);
            if stored != computed {
                return Err(Error::ChecksumMismatch {
                    what: "inode",
                    block: ino,
                });
            }
            // A v3 inode records its own number and its filesystem's
            // UUID. Both catch an inode read from the wrong offset, or
            // one left behind by a previous filesystem — neither of
            // which the checksum can detect, since such a block is
            // internally perfect.
            let self_ino = be64(buf, 152);
            if self_ino != ino {
                return Err(Error::BlockIdentityMismatch {
                    what: "inode",
                    expected: ino,
                    found: self_ino,
                });
            }
            let uuid: [u8; 16] = buf[160..176].try_into().expect("16 bytes");
            if uuid != sb.meta_uuid {
                return Err(Error::BlockIdentityMismatch {
                    what: "inode",
                    expected: ino,
                    found: u64::MAX,
                });
            }
        }

        let flags2 = if is_v3 { be64(buf, 120) } else { 0 };
        let bigtime = flags2 & flags2::BIGTIME != 0;

        // With the 64-bit extent counter feature the fields at 24 and 76
        // widen, displacing the attribute count. Without it, 24 is
        // padding and the counts sit at 76 and 80.
        let nrext64 = sb.features_incompat & crate::superblock::incompat::NREXT64 != 0;
        let (nextents, anextents) = if nrext64 {
            (be64(buf, 24), be32(buf, 76))
        } else {
            (u64::from(be32(buf, 76)), u32::from(be16(buf, 80)))
        };

        let inode = Inode {
            ino,
            mode: be16(buf, 2),
            version,
            format: Format::from_raw(buf[5])?,
            aformat: Format::from_raw(buf[83])?,
            uid: be32(buf, 8),
            gid: be32(buf, 12),
            nlink: be32(buf, 16),
            size: be64(buf, 56),
            nblocks: be64(buf, 64),
            nextents,
            anextents,
            forkoff: buf[82],
            flags: be16(buf, 90),
            flags2,
            gen: be32(buf, 92),
            next_unlinked: be32(buf, 96),
            atime: Timestamp::parse(buf, 32, bigtime),
            mtime: Timestamp::parse(buf, 40, bigtime),
            ctime: Timestamp::parse(buf, 48, bigtime),
            crtime: if is_v3 {
                Timestamp::parse(buf, 144, bigtime)
            } else {
                Timestamp { sec: 0, nsec: 0 }
            },
        };

        inode.validate(sb)?;
        Ok(inode)
    }

    /// Structural checks that catch a misread core.
    fn validate(&self, sb: &Superblock) -> Result<()> {
        let isize = u64::from(sb.inodesize);

        // The attribute fork offset is measured in 8-byte units from the
        // end of the core; it cannot run past the end of the inode.
        if self.forkoff != 0 {
            let core = if self.version == 3 {
                XFS_DINODE_V3_SIZE
            } else {
                XFS_DINODE_V2_SIZE
            } as u64;
            let attr_start = core + u64::from(self.forkoff) * 8;
            if attr_start > isize {
                return Err(Error::BadSuperblock(format!(
                    "inode {}: attribute fork starts at {attr_start}, past the {isize}-byte inode",
                    self.ino
                )));
            }
        }
        // A local-format fork stores its data inside the inode, so the
        // size cannot exceed what the inode can hold.
        if self.format == Format::Local && self.size > isize {
            return Err(Error::BadSuperblock(format!(
                "inode {}: local-format fork claims {} bytes, larger than the {isize}-byte inode",
                self.ino, self.size
            )));
        }
        // A device inode has no blocks and no extents.
        if self.format == Format::Dev && (self.nblocks != 0 || self.nextents != 0) {
            return Err(Error::BadSuperblock(format!(
                "inode {}: device inode has {} blocks and {} extents",
                self.ino, self.nblocks, self.nextents
            )));
        }
        Ok(())
    }

    /// Decode the file type from the mode's format bits.
    pub fn file_type(&self) -> Option<FileType> {
        Some(match self.mode & 0xF000 {
            0x8000 => FileType::Regular,
            0x4000 => FileType::Directory,
            0xA000 => FileType::Symlink,
            0x2000 => FileType::CharDevice,
            0x6000 => FileType::BlockDevice,
            0x1000 => FileType::Fifo,
            0xC000 => FileType::Socket,
            _ => return None,
        })
    }

    /// Permission bits only.
    pub fn permissions(&self) -> u16 {
        self.mode & 0o7777
    }

    /// Whether this inode is a directory.
    pub fn is_dir(&self) -> bool {
        self.file_type() == Some(FileType::Directory)
    }

    /// Whether this inode is a regular file.
    pub fn is_regular_file(&self) -> bool {
        self.file_type() == Some(FileType::Regular)
    }

    /// Whether this inode is a symbolic link.
    pub fn is_symlink(&self) -> bool {
        self.file_type() == Some(FileType::Symlink)
    }

    /// Byte offset of the data fork within the inode record.
    pub fn data_fork_offset(&self) -> usize {
        if self.version == 3 {
            XFS_DINODE_V3_SIZE
        } else {
            XFS_DINODE_V2_SIZE
        }
    }

    /// Byte range of the data fork within the inode record, given the
    /// inode size. The data fork runs to the start of the attribute fork
    /// when one is present, and to the end of the inode otherwise.
    pub fn data_fork_range(&self, inodesize: usize) -> (usize, usize) {
        let start = self.data_fork_offset();
        let end = if self.forkoff == 0 {
            inodesize
        } else {
            start + usize::from(self.forkoff) * 8
        };
        (start, end.min(inodesize))
    }

    /// Byte range of the attribute fork, or `None` when absent.
    pub fn attr_fork_range(&self, inodesize: usize) -> Option<(usize, usize)> {
        if self.forkoff == 0 {
            return None;
        }
        let start = self.data_fork_offset() + usize::from(self.forkoff) * 8;
        if start >= inodesize {
            return None;
        }
        Some((start, inodesize))
    }

    /// Whether any of this inode's extents may be shared with another
    /// inode through reflink. Shared extents must never be written in
    /// place.
    pub fn has_shared_extents(&self) -> bool {
        self.flags2 & flags2::REFLINK != 0
    }

    /// Whether this inode's data lives on the real-time device, which
    /// this driver does not address.
    pub fn is_realtime(&self) -> bool {
        self.flags & flags::REALTIME != 0
    }
}

#[cfg(test)]
mod tests {
    //! These fixtures are built in-process, so they prove the parser is
    //! self-consistent and nothing more. Correctness is established by
    //! `tests/oracle_vm_fixtures.rs`, which compares this parser against
    //! real filesystems built by `mkfs.xfs`. See AGENTS.md.

    use super::*;
    use crate::superblock::XFS_SB_MAGIC;

    fn sb_v5() -> Superblock {
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&XFS_SB_MAGIC.to_be_bytes());
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
        for (i, slot) in b[32..48].iter_mut().enumerate() {
            *slot = i as u8;
        }
        let crc = crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).unwrap()
    }

    /// A v3 directory inode, 512 bytes, correctly checksummed.
    fn v3_inode(sb: &Superblock, ino: u64) -> Vec<u8> {
        let mut b = vec![0u8; usize::from(sb.inodesize)];
        b[0..2].copy_from_slice(&XFS_DINODE_MAGIC.to_be_bytes());
        b[2..4].copy_from_slice(&0o040755u16.to_be_bytes()); // dir, rwxr-xr-x
        b[4] = 3; // version
        b[5] = 1; // format: local
        b[8..12].copy_from_slice(&1000u32.to_be_bytes()); // uid
        b[12..16].copy_from_slice(&1000u32.to_be_bytes()); // gid
        b[16..20].copy_from_slice(&2u32.to_be_bytes()); // nlink
        b[32..36].copy_from_slice(&1_700_000_000u32.to_be_bytes()); // atime sec
        b[40..44].copy_from_slice(&1_700_000_001u32.to_be_bytes()); // mtime sec
        b[48..52].copy_from_slice(&1_700_000_002u32.to_be_bytes()); // ctime sec
        b[56..64].copy_from_slice(&64u64.to_be_bytes()); // size
        b[83] = 2; // aformat: extents
        b[92..96].copy_from_slice(&7u32.to_be_bytes()); // gen
        b[96..100].copy_from_slice(&u32::MAX.to_be_bytes()); // next_unlinked
        b[144..148].copy_from_slice(&1_699_999_999u32.to_be_bytes()); // crtime
        b[152..160].copy_from_slice(&ino.to_be_bytes());
        b[160..176].copy_from_slice(&sb.meta_uuid);
        let crc = crc32c_with_zeroed_crc(&b, DI_CRC_OFFSET);
        b[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        b
    }

    #[test]
    fn parses_v3_directory_inode() {
        let sb = sb_v5();
        let inode = Inode::parse(&v3_inode(&sb, 128), &sb, 128).unwrap();
        assert_eq!(inode.ino, 128);
        assert_eq!(inode.version, 3);
        assert_eq!(inode.format, Format::Local);
        assert_eq!(inode.uid, 1000);
        assert_eq!(inode.nlink, 2);
        assert_eq!(inode.size, 64);
        assert_eq!(inode.gen, 7);
        assert!(inode.is_dir());
        assert_eq!(inode.permissions(), 0o755);
        assert_eq!(inode.file_type(), Some(FileType::Directory));
    }

    #[test]
    fn decodes_timestamps() {
        let sb = sb_v5();
        let inode = Inode::parse(&v3_inode(&sb, 128), &sb, 128).unwrap();
        assert_eq!(inode.atime.sec, 1_700_000_000);
        assert_eq!(inode.mtime.sec, 1_700_000_001);
        assert_eq!(inode.ctime.sec, 1_700_000_002);
    }

    /// An inode read from the wrong offset is internally perfect but
    /// records a different number. The checksum cannot catch this.
    #[test]
    fn rejects_inode_with_wrong_self_number() {
        let sb = sb_v5();
        let buf = v3_inode(&sb, 128);
        match Inode::parse(&buf, &sb, 129) {
            Err(Error::BlockIdentityMismatch {
                expected, found, ..
            }) => {
                assert_eq!(expected, 129);
                assert_eq!(found, 128);
            }
            other => panic!("expected identity mismatch, got {other:?}"),
        }
    }

    /// An inode left behind by a previous filesystem on the same device.
    #[test]
    fn rejects_inode_from_foreign_filesystem() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[160] ^= 0xFF;
        let crc = crc32c_with_zeroed_crc(&buf, DI_CRC_OFFSET);
        buf[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Inode::parse(&buf, &sb, 128),
            Err(Error::BlockIdentityMismatch { .. })
        ));
    }

    #[test]
    fn rejects_bad_crc() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[56] ^= 0xFF; // change the size without fixing the checksum
        assert!(matches!(
            Inode::parse(&buf, &sb, 128),
            Err(Error::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn rejects_wrong_magic() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[0..2].copy_from_slice(&0xDEADu16.to_be_bytes());
        assert!(matches!(
            Inode::parse(&buf, &sb, 128),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_undefined_fork_format() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[5] = 9;
        let crc = crc32c_with_zeroed_crc(&buf, DI_CRC_OFFSET);
        buf[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Inode::parse(&buf, &sb, 128),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn rejects_attr_fork_past_end_of_inode() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[82] = 200; // 200 * 8 = 1600 bytes past a 512-byte inode
        let crc = crc32c_with_zeroed_crc(&buf, DI_CRC_OFFSET);
        buf[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        assert!(matches!(
            Inode::parse(&buf, &sb, 128),
            Err(Error::BadSuperblock(_))
        ));
    }

    #[test]
    fn fork_ranges_without_attributes() {
        let sb = sb_v5();
        let inode = Inode::parse(&v3_inode(&sb, 128), &sb, 128).unwrap();
        assert_eq!(inode.data_fork_range(512), (XFS_DINODE_V3_SIZE, 512));
        assert_eq!(inode.attr_fork_range(512), None);
    }

    #[test]
    fn fork_ranges_with_attributes() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[82] = 20; // attribute fork 160 bytes past the core
        let crc = crc32c_with_zeroed_crc(&buf, DI_CRC_OFFSET);
        buf[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        let inode = Inode::parse(&buf, &sb, 128).unwrap();
        let split = XFS_DINODE_V3_SIZE + 160;
        assert_eq!(inode.data_fork_range(512), (XFS_DINODE_V3_SIZE, split));
        assert_eq!(inode.attr_fork_range(512), Some((split, 512)));
    }

    #[test]
    fn bigtime_timestamps_use_the_1901_epoch() {
        let sb = sb_v5();
        let mut buf = v3_inode(&sb, 128);
        buf[120..128].copy_from_slice(&flags2::BIGTIME.to_be_bytes());
        // One second past the bigtime epoch.
        buf[32..40].copy_from_slice(&NSEC_PER_SEC.to_be_bytes());
        let crc = crc32c_with_zeroed_crc(&buf, DI_CRC_OFFSET);
        buf[DI_CRC_OFFSET..DI_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
        let inode = Inode::parse(&buf, &sb, 128).unwrap();
        assert_eq!(inode.atime.sec, BIGTIME_EPOCH_OFFSET_SECS + 1);
        assert_eq!(inode.atime.nsec, 0);
    }
}
