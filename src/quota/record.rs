//! XFS quota records (`xfs_dqblk`), read without changing accounting.
//!
//! Layout: XFS Algorithms & Data Structures, Disk Quotas,
//! <https://www.kernel.org/pub/linux/utils/fs/xfs/docs/xfs_filesystem_structure.pdf>.
//! Each filesystem block holds `blocksize / 136` records, then slack.
//! IDs are indexed by logical block and slot, not by byte offset / 136.
//! The v5 CRC covers one complete 136-byte record, with its little-endian
//! checksum at 108 zeroed. Its UUID is the metadata UUID at 120.

use crate::endian::{be16, be32, be64, le32};
use crate::{Error, Result, Superblock};

/// Size of a disk quota record on both v4 and v5.
pub const RECORD_SIZE: usize = 136;

/// The namespace a quota ID belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// User ID.
    User,
    /// Group ID.
    Group,
    /// Project ID.
    Project,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Group => "group",
            Self::Project => "project",
        }
    }
    pub(crate) fn record_type(self) -> u8 {
        match self {
            Self::User => 1,
            Self::Group => 4,
            Self::Project => 2,
        }
    }
    pub(crate) fn accounting(self) -> u16 {
        match self {
            Self::User => 0x1,
            Self::Group => 0x40,
            Self::Project => 0x8,
        }
    }
    pub(crate) fn checked(self, v5: bool) -> u16 {
        match self {
            Self::User => 0x4,
            Self::Group if v5 => 0x100,
            Self::Project if v5 => 0x400,
            _ => 0x20,
        }
    }
    pub(crate) fn inode(self, sb: &Superblock) -> u64 {
        match self {
            Self::User => sb.uquotino,
            Self::Group => {
                if !sb.is_v5() && sb.qflags & 0x8 != 0 {
                    u64::MAX
                } else {
                    sb.gquotino
                }
            }
            Self::Project => {
                if sb.is_v5() {
                    sb.pquotino
                } else if sb.qflags & 0x8 != 0 {
                    sb.gquotino
                } else {
                    u64::MAX
                }
            }
        }
    }
}

/// A quota resource. Block quantities use filesystem blocks, not 512-byte units.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resource {
    /// Recorded usage.
    pub count: u64,
    /// Zero means no soft limit.
    pub soft_limit: u64,
    /// Zero means no hard limit.
    pub hard_limit: u64,
    /// Raw grace expiration; ID zero holds default grace durations instead.
    pub timer: u32,
}

/// One quota ID's on-disk limits and counters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// The ID named by this record.
    pub id: u32,
    /// User, group or project namespace.
    pub kind: Kind,
    /// Data-device blocks, including attributes and extent-tree blocks.
    pub blocks: Resource,
    /// Allocated inodes.
    pub inodes: Resource,
    /// Realtime data blocks.
    pub realtime: Resource,
    /// Timers use the bigtime representation.
    pub bigtime: bool,
}

impl Record {
    /// Parse and verify a record against its quota inode's namespace and slot.
    /// On v5, `metadata_uuid` enables both CRC and UUID verification. On v4
    /// pass `None`; the reserved v5 extension is not interpreted.
    pub fn parse(
        bytes: &[u8],
        kind: Kind,
        id: u32,
        metadata_uuid: Option<&[u8; 16]>,
        bigtime: bool,
    ) -> Result<Self> {
        let bad = |reason: &str| {
            Error::BadSuperblock(format!("{} quota record {id}: {reason}", kind.name()))
        };
        if bytes.len() < RECORD_SIZE {
            return Err(bad("truncated record"));
        }
        let bytes = &bytes[..RECORD_SIZE];
        if let Some(uuid) = metadata_uuid {
            if le32(bytes, 108) != crate::superblock::crc32c_with_zeroed_crc(bytes, 108) {
                return Err(bad("checksum mismatch"));
            }
            if &bytes[120..136] != uuid {
                return Err(bad("metadata UUID mismatch"));
            }
        }
        if be16(bytes, 0) != 0x4451 {
            return Err(bad("bad magic"));
        }
        if bytes[2] != 1 {
            return Err(bad("bad version"));
        }
        if bytes[3] & !0x80 != kind.record_type() {
            return Err(bad("wrong quota type"));
        }
        if be32(bytes, 4) != id {
            return Err(bad("ID does not match its logical block and slot"));
        }
        let has_bigtime = bytes[3] & 0x80 != 0;
        if has_bigtime && (metadata_uuid.is_none() || !bigtime || id == 0) {
            return Err(bad("invalid bigtime flag"));
        }
        let record = Self {
            id,
            kind,
            bigtime: has_bigtime,
            blocks: Resource {
                hard_limit: be64(bytes, 8),
                soft_limit: be64(bytes, 16),
                count: be64(bytes, 40),
                timer: be32(bytes, 60),
            },
            inodes: Resource {
                hard_limit: be64(bytes, 24),
                soft_limit: be64(bytes, 32),
                count: be64(bytes, 48),
                timer: be32(bytes, 56),
            },
            realtime: Resource {
                hard_limit: be64(bytes, 72),
                soft_limit: be64(bytes, 80),
                count: be64(bytes, 88),
                timer: be32(bytes, 96),
            },
        };
        for (name, resource) in [
            ("blocks", record.blocks),
            ("inodes", record.inodes),
            ("realtime blocks", record.realtime),
        ] {
            if resource.hard_limit != 0 && resource.soft_limit > resource.hard_limit {
                return Err(Error::BadSuperblock(format!(
                    "{} quota record {id}: {name} soft limit exceeds hard limit",
                    kind.name()
                )));
            }
            // ID zero contains defaults, and a privileged write may exceed a
            // hard limit. Neither condition makes the record corrupt.
            if id != 0
                && resource.soft_limit != 0
                && resource.count > resource.soft_limit
                && resource.timer == 0
            {
                return Err(Error::BadSuperblock(format!(
                    "{} quota record {id}: {name} exceeds soft limit without a grace timer",
                    kind.name()
                )));
            }
        }
        Ok(record)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quota_modes_select_the_versioned_inode_slots_and_checked_flags() {
        let mut sb = crate::mkfs::plan(400 << 20, &crate::mkfs::Options::default())
            .unwrap()
            .superblock()
            .clone();
        sb.uquotino = 131;
        sb.gquotino = 132;
        sb.pquotino = 133;
        for (kind, ino, accounting, checked) in [
            (Kind::User, 131, 1, 4),
            (Kind::Group, 132, 0x40, 0x100),
            (Kind::Project, 133, 8, 0x400),
        ] {
            assert_eq!(kind.inode(&sb), ino);
            assert_eq!(kind.accounting(), accounting);
            assert_eq!(kind.checked(true), checked);
        }
        sb.versionnum = 4;
        sb.qflags = 0x40;
        assert_eq!(Kind::Group.inode(&sb), 132);
        assert_eq!(Kind::Project.inode(&sb), u64::MAX);
        sb.qflags = 8;
        assert_eq!(Kind::Group.inode(&sb), u64::MAX);
        assert_eq!(Kind::Project.inode(&sb), 132);
        assert_eq!(Kind::User.inode(&sb), 131);
        assert_eq!(Kind::User.checked(false), 4);
        assert_eq!(Kind::Group.checked(false), 0x20);
        assert_eq!(Kind::Project.checked(false), 0x20);
    }
    fn bytes(kind: Kind, id: u32) -> [u8; RECORD_SIZE] {
        let mut b = [0; RECORD_SIZE];
        b[..2].copy_from_slice(&0x4451u16.to_be_bytes());
        b[2] = 1;
        b[3] = kind.record_type();
        b[4..8].copy_from_slice(&id.to_be_bytes());
        b
    }
    fn crc(b: &mut [u8]) {
        let c = crate::superblock::crc32c_with_zeroed_crc(b, 108);
        b[108..112].copy_from_slice(&c.to_le_bytes());
    }
    #[test]
    fn identities_and_record_bounds_are_checked() {
        for kind in [Kind::User, Kind::Group, Kind::Project] {
            let b = bytes(kind, 17);
            assert!(Record::parse(&b, kind, 17, None, false).is_ok());
            for n in 0..RECORD_SIZE {
                assert!(Record::parse(&b[..n], kind, 17, None, false).is_err());
            }
            assert!(Record::parse(&b, kind, 18, None, false).is_err());
            for (at, value) in [
                (0, 0),
                (2, 2),
                (3, 0),
                (3, 7),
                (3, 0x40),
                (3, kind.record_type() | 0x80),
            ] {
                let mut bad = b;
                bad[at] = value;
                assert!(Record::parse(&bad, kind, 17, None, false).is_err());
            }
        }
    }
    #[test]
    fn v5_checks_crc_uuid_and_bigtime_independently() {
        let uuid = [0x52; 16];
        let mut b = bytes(Kind::Project, 65553);
        b[3] |= 0x80;
        b[120..].copy_from_slice(&uuid);
        crc(&mut b);
        assert!(
            Record::parse(&b, Kind::Project, 65553, Some(&uuid), true)
                .unwrap()
                .bigtime
        );
        assert!(Record::parse(&b, Kind::Project, 65553, Some(&uuid), false).is_err());
        assert!(Record::parse(&b, Kind::Project, 65553, Some(&[0; 16]), true).is_err());
        b[40] ^= 1;
        assert!(Record::parse(&b, Kind::Project, 65553, Some(&uuid), true).is_err());
        let mut b = bytes(Kind::User, 0);
        b[3] |= 0x80;
        b[120..].copy_from_slice(&uuid);
        crc(&mut b);
        assert!(Record::parse(&b, Kind::User, 0, Some(&uuid), true).is_err());
    }
    #[test]
    fn limits_and_grace_timers_have_root_and_unlimited_exceptions() {
        for (hard, soft, count, timer) in [(8, 16, 40, 60), (24, 32, 48, 56), (72, 80, 88, 96)] {
            let mut b = bytes(Kind::User, 17);
            b[hard..hard + 8].copy_from_slice(&2u64.to_be_bytes());
            b[soft..soft + 8].copy_from_slice(&3u64.to_be_bytes());
            assert!(Record::parse(&b, Kind::User, 17, None, false).is_err());
            b[soft..soft + 8].copy_from_slice(&1u64.to_be_bytes());
            b[count..count + 8].copy_from_slice(&3u64.to_be_bytes());
            assert!(Record::parse(&b, Kind::User, 17, None, false).is_err());
            b[timer..timer + 4].copy_from_slice(&1u32.to_be_bytes());
            assert!(Record::parse(&b, Kind::User, 17, None, false).is_ok());
            b[timer..timer + 4].fill(0);
            b[4..8].fill(0);
            assert!(Record::parse(&b, Kind::User, 0, None, false).is_ok());
            b[4..8].copy_from_slice(&17u32.to_be_bytes());
            b[soft..soft + 8].fill(0);
            b[hard..hard + 8].fill(0);
            assert!(Record::parse(&b, Kind::User, 17, None, false).is_ok());
        }
    }
}
