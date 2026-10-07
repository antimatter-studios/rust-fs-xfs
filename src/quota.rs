//! XFS quota-record accounting helpers.

use crate::error::{Error, Result};
use crate::fs::Filesystem;
use crate::log_write::Op;
use std::collections::BTreeMap;

/// Bytes in `struct xfs_dqblk` on disk.
pub(crate) const DQBLK_SIZE: usize = 136;
pub(crate) const DQ_MAGIC: u16 = 0x4451;
pub(crate) const DQ_VERSION: u8 = 1;
pub(crate) const DQ_USER: u8 = 1;
pub(crate) const DQ_PROJECT: u8 = 2;
pub(crate) const DQ_GROUP: u8 = 4;
pub(crate) const XFS_LI_DQUOT: u16 = 0x123d;

const DQ_BHARD: usize = 8;
const DQ_IHARD: usize = 24;
const DQ_BCOUNT: usize = 40;
const DQ_ICOUNT: usize = 48;
pub(crate) const DQ_CRC: usize = 108;

pub(crate) fn project_id(raw: &[u8]) -> u32 {
    (u32::from(crate::endian::be16(
        raw,
        crate::format::log_items::log_dinode::offsets::PROJID_HI,
    )) << 16)
        | u32::from(crate::endian::be16(
            raw,
            crate::format::log_items::log_dinode::offsets::PROJID_LO,
        ))
}

#[derive(Clone, Copy)]
pub(crate) struct QuotaChange {
    pub uid: u32,
    pub gid: u32,
    pub project_id: u32,
    pub blocks_512: i64,
    pub inodes: i64,
}

#[derive(Clone)]
pub(crate) struct QuotaLogItem {
    blkno: u64,
    offset: u32,
    id: u32,
    record: Vec<u8>,
}

impl QuotaLogItem {
    pub(crate) fn op_count(&self) -> usize {
        2
    }

    pub(crate) fn ops(&self) -> [Op; 2] {
        let mut format = vec![0u8; 24];
        format[0..2].copy_from_slice(&XFS_LI_DQUOT.to_ne_bytes());
        format[2..4].copy_from_slice(&2u16.to_ne_bytes());
        format[4..8].copy_from_slice(&self.id.to_ne_bytes());
        format[8..16].copy_from_slice(&(self.blkno as i64).to_ne_bytes());
        format[16..20].copy_from_slice(&1i32.to_ne_bytes());
        format[20..24].copy_from_slice(&self.offset.to_ne_bytes());
        [
            Op {
                flags: 0,
                data: format,
            },
            Op {
                flags: 0,
                data: self.record.clone(),
            },
        ]
    }

    pub(crate) fn apply_overlay(&self, fs: &Filesystem) -> Result<()> {
        let at = self.blkno * 512;
        let mut block = vec![0u8; fs.sb.blocksize as usize];
        fs.device.read_at(at, &mut block)?;
        let start = self.offset as usize;
        block[start..start + self.record.len()].copy_from_slice(&self.record);
        fs.logged_buffer(at, &block);
        Ok(())
    }
}

/// Build journal buffer items for enabled quota types. Changes to the same
/// dquot are combined first, preventing two stale copies of one buffer from
/// overwriting each other in a single transaction.
pub(crate) fn accounting_items(
    fs: &Filesystem,
    changes: &[QuotaChange],
) -> Result<Vec<QuotaLogItem>> {
    let mut deltas = BTreeMap::<(u64, u32, u8, bool), (i64, i64)>::new();

    const UACCT: u16 = 1 << 0;
    const UENFD: u16 = 1 << 1;
    const PACCT: u16 = 1 << 3;
    const PENFD: u16 = 1 << 9;
    const GACCT: u16 = 1 << 6;
    const GENFD: u16 = 1 << 7;

    for change in changes {
        if change.blocks_512 == 0 && change.inodes == 0 {
            continue;
        }
        let owners = [
            (UACCT, UENFD, fs.sb.uquotino, change.uid, DQ_USER),
            (GACCT, GENFD, fs.sb.gquotino, change.gid, DQ_GROUP),
            (PACCT, PENFD, fs.sb.pquotino, change.project_id, DQ_PROJECT),
        ];
        for (acct, enfd, qino, id, kind) in owners {
            if fs.sb.qflags & acct == 0 {
                continue;
            }
            if qino == 0 {
                return Err(Error::BadSuperblock(format!(
                    "quota accounting flag {acct:#x} is set without a quota inode"
                )));
            }
            // XFS always accounts root, but never enforces limits on
            // user, group, or project ID zero.
            let enforce = fs.sb.qflags & enfd != 0 && id != 0;
            let delta = deltas.entry((qino, id, kind, enforce)).or_default();
            delta.0 = delta
                .0
                .checked_add(change.blocks_512)
                .ok_or_else(|| Error::UnsupportedFeature("quota block delta overflowed".into()))?;
            delta.1 = delta
                .1
                .checked_add(change.inodes)
                .ok_or_else(|| Error::UnsupportedFeature("quota inode delta overflowed".into()))?;
        }
    }
    deltas
        .into_iter()
        .map(|((qino, id, kind, enforce), (blocks_512, inodes))| {
            update_dquot(fs, qino, id, kind, blocks_512, inodes, enforce)
        })
        .collect()
}

fn update_dquot(
    fs: &Filesystem,
    qino: u64,
    id: u32,
    kind: u8,
    blocks_512: i64,
    inodes: i64,
    enforce: bool,
) -> Result<QuotaLogItem> {
    let (qfile, qraw) = fs.read_inode_raw(qino)?;
    let file_offset = u64::from(id)
        .checked_mul(DQBLK_SIZE as u64)
        .ok_or_else(|| Error::UnsupportedFeature("quota record offset overflowed".into()))?;
    if file_offset + DQBLK_SIZE as u64 > qfile.size {
        return Err(Error::UnsupportedFeature(format!(
            "quota inode {qino} ends before record {id}"
        )));
    }
    let blocksize = u64::from(fs.sb.blocksize);
    let logical = file_offset / blocksize;
    let within = (file_offset % blocksize) as usize;
    if within + DQBLK_SIZE > blocksize as usize {
        return Err(Error::UnsupportedFeature(format!(
            "quota record {id} crosses a filesystem block and cannot be journalled safely"
        )));
    }
    let extents = fs.data_extents(&qfile, &qraw)?;
    let extent = crate::extent::lookup(&extents, logical).ok_or_else(|| {
        Error::UnsupportedFeature(format!(
            "quota record {id} is not allocated in quota inode {qino}"
        ))
    })?;
    let fsblock = extent
        .map(logical)
        .ok_or_else(|| Error::Internal("quota extent lookup lost its mapping".into()))?;
    let mut block = fs.read_fsblock(fsblock)?;
    let end = within + DQBLK_SIZE;
    adjust_record(
        &mut block[within..end],
        kind,
        id,
        blocks_512,
        inodes,
        enforce,
    )?;
    let blkno = fsblock
        .checked_mul(blocksize / 512)
        .ok_or_else(|| Error::UnsupportedFeature("quota buffer address overflowed".into()))?;
    Ok(QuotaLogItem {
        blkno,
        offset: within as u32,
        id,
        record: block[within..end].to_vec(),
    })
}

/// Apply usage changes to one existing dquot. `blocks_512` is measured
/// in XFS basic blocks (512-byte units), as are the on-disk limits.
/// Nothing in the record changes when validation or enforcement fails.
pub(crate) fn adjust_record(
    record: &mut [u8],
    kind: u8,
    id: u32,
    blocks_512: i64,
    inodes: i64,
    enforce: bool,
) -> Result<()> {
    if record.len() != DQBLK_SIZE {
        return Err(Error::UnsupportedFeature(format!(
            "XFS dquot record is {} bytes; expected {DQBLK_SIZE}",
            record.len()
        )));
    }
    if record.iter().all(|byte| *byte == 0) {
        record[0..2].copy_from_slice(&DQ_MAGIC.to_be_bytes());
        record[2] = DQ_VERSION;
        record[3] = kind;
        record[4..8].copy_from_slice(&id.to_be_bytes());
    } else if u16::from_be_bytes(record[0..2].try_into().expect("two bytes")) != DQ_MAGIC
        || record[2] != DQ_VERSION
        || record[3] & 0x07 != kind
        || u32::from_be_bytes(record[4..8].try_into().expect("four bytes")) != id
    {
        return Err(Error::UnsupportedFeature(format!(
            "quota record for {kind:#x} id {id} is absent or malformed"
        )));
    } else {
        let stored = u32::from_le_bytes(record[DQ_CRC..DQ_CRC + 4].try_into().unwrap());
        let computed = crate::superblock::crc32c_with_zeroed_crc(record, DQ_CRC);
        if stored != computed {
            return Err(Error::UnsupportedFeature(format!(
                "quota record for {kind:#x} id {id} has a bad CRC"
            )));
        }
    }

    let old_blocks = u64::from_be_bytes(record[DQ_BCOUNT..DQ_BCOUNT + 8].try_into().unwrap());
    let old_inodes = u64::from_be_bytes(record[DQ_ICOUNT..DQ_ICOUNT + 8].try_into().unwrap());
    let next_blocks = usage_after(old_blocks, blocks_512, "block")?;
    let next_inodes = usage_after(old_inodes, inodes, "inode")?;
    let block_limit = u64::from_be_bytes(record[DQ_BHARD..DQ_BHARD + 8].try_into().unwrap());
    let inode_limit = u64::from_be_bytes(record[DQ_IHARD..DQ_IHARD + 8].try_into().unwrap());
    if enforce && block_limit != 0 && next_blocks > block_limit {
        return Err(Error::UnsupportedFeature(format!(
            "quota {kind:#x} id {id} block hard limit {block_limit} would be exceeded by {next_blocks}"
        )));
    }
    if enforce && inode_limit != 0 && next_inodes > inode_limit {
        return Err(Error::UnsupportedFeature(format!(
            "quota {kind:#x} id {id} inode hard limit {inode_limit} would be exceeded by {next_inodes}"
        )));
    }

    record[DQ_BCOUNT..DQ_BCOUNT + 8].copy_from_slice(&next_blocks.to_be_bytes());
    record[DQ_ICOUNT..DQ_ICOUNT + 8].copy_from_slice(&next_inodes.to_be_bytes());
    let crc = crate::superblock::crc32c_with_zeroed_crc(record, DQ_CRC);
    record[DQ_CRC..DQ_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    Ok(())
}

fn usage_after(current: u64, delta: i64, what: &str) -> Result<u64> {
    let next = i128::from(current) + i128::from(delta);
    u64::try_from(next).map_err(|_| {
        Error::UnsupportedFeature(format!(
            "quota {what} usage {current} cannot be adjusted by {delta}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seal(record: &mut [u8]) {
        let crc = crate::superblock::crc32c_with_zeroed_crc(record, DQ_CRC);
        record[DQ_CRC..DQ_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    }

    #[test]
    fn block_usage_is_updated_in_512_byte_units_and_crc_is_refreshed() {
        let mut record = vec![0u8; DQBLK_SIZE];
        record[0..2].copy_from_slice(&DQ_MAGIC.to_be_bytes());
        record[2] = DQ_VERSION;
        record[3] = DQ_USER;
        record[4..8].copy_from_slice(&7u32.to_be_bytes());
        record[8..16].copy_from_slice(&10u64.to_be_bytes());
        record[40..48].copy_from_slice(&2u64.to_be_bytes());
        seal(&mut record);

        adjust_record(&mut record, DQ_USER, 7, 1, 0, true).unwrap();

        assert_eq!(u64::from_be_bytes(record[40..48].try_into().unwrap()), 3);
        assert_eq!(
            u32::from_le_bytes(record[108..112].try_into().unwrap()),
            crate::superblock::crc32c_with_zeroed_crc(&record, 108)
        );
    }

    #[test]
    fn hard_block_limit_refuses_without_changing_the_record() {
        let mut record = vec![0u8; DQBLK_SIZE];
        record[0..2].copy_from_slice(&DQ_MAGIC.to_be_bytes());
        record[2] = DQ_VERSION;
        record[3] = DQ_USER;
        record[4..8].copy_from_slice(&7u32.to_be_bytes());
        record[8..16].copy_from_slice(&2u64.to_be_bytes());
        record[40..48].copy_from_slice(&2u64.to_be_bytes());
        seal(&mut record);
        let before = record.clone();

        assert!(adjust_record(&mut record, DQ_USER, 7, 1, 0, true).is_err());
        assert_eq!(record, before);
    }
}
