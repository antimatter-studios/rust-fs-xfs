//! XFS quota-record accounting helpers.

mod record;
pub use record::{Kind, Record, Resource, RECORD_SIZE};

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
    /// Filesystem blocks, matching Linux's dquot counters and limits.
    pub blocks_fs: i64,
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
        if change.blocks_fs == 0 && change.inodes == 0 {
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
                .checked_add(change.blocks_fs)
                .ok_or_else(|| Error::UnsupportedFeature("quota block delta overflowed".into()))?;
            delta.1 = delta
                .1
                .checked_add(change.inodes)
                .ok_or_else(|| Error::UnsupportedFeature("quota inode delta overflowed".into()))?;
        }
    }
    deltas
        .into_iter()
        .map(|((qino, id, kind, enforce), (blocks_fs, inodes))| {
            update_dquot(fs, qino, id, kind, blocks_fs, inodes, enforce)
        })
        .collect()
}

fn record_address(blocksize: u32, id: u32) -> Result<(u64, usize)> {
    // Linux xfs_dquot_alloc addresses one filesystem-block quota cluster
    // by ID / qi_dqperchunk, with ID % qi_dqperchunk selecting its record.
    // The unused bytes at each block's end are padding, not another dquot.
    let per_block = blocksize as usize / DQBLK_SIZE;
    if per_block == 0 {
        return Err(Error::UnsupportedFeature(
            "filesystem block is too small for an XFS dquot".into(),
        ));
    }
    Ok((
        u64::from(id) / per_block as u64,
        (u64::from(id) % per_block as u64) as usize * DQBLK_SIZE,
    ))
}

fn update_dquot(
    fs: &Filesystem,
    qino: u64,
    id: u32,
    kind: u8,
    blocks_fs: i64,
    inodes: i64,
    enforce: bool,
) -> Result<QuotaLogItem> {
    let (qfile, qraw) = fs.read_inode_raw(qino)?;
    // Quota inodes are internal sparse metadata files. Linux maps their
    // extents directly without updating or bounding reads by di_size.
    // A missing cluster remains an error below; its allocation is separate.
    let blocksize = u64::from(fs.sb.blocksize);
    let (logical, within) = record_address(fs.sb.blocksize, id)?;
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
        blocks_fs,
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

/// Apply usage changes to one existing dquot. `blocks_fs`, on-disk usage,
/// and both hard and soft block limits are measured in filesystem blocks.
/// Linux v6.1 xfs_qm_scall_setqlim converts byte limits with XFS_B_TO_FSB;
/// xfs_dquot_to_disk stores those counters directly, and xfs_trans_dquot
/// applies transaction block deltas without a basic-block conversion.
/// Nothing in the record changes when validation or enforcement fails.
pub(crate) fn adjust_record(
    record: &mut [u8],
    kind: u8,
    id: u32,
    blocks_fs: i64,
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
    let next_blocks = usage_after(old_blocks, blocks_fs, "block")?;
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

    #[test]
    fn linux_dquot_chunk_addresses_keep_padding_between_blocks() {
        // Linux v6.1 xfs_dquot_alloc divides IDs by qi_dqperchunk and
        // takes the remainder for q_bufoffset; a 4 KiB chunk holds 30
        // complete 136-byte xfs_dqblk records, followed by 16 pad bytes.
        assert_eq!(record_address(4096, 29).unwrap(), (0, 3944));
        assert_eq!(record_address(4096, 30).unwrap(), (1, 0));
        assert_eq!(record_address(4096, 65534).unwrap(), (2184, 1904));
        assert_eq!(record_address(1024, 7).unwrap(), (1, 0));
        assert_eq!(record_address(2048, 15).unwrap(), (1, 0));
        assert_eq!(record_address(4096, u32::MAX).unwrap(), (143165576, 2040));
    }

    #[test]
    fn blocks_too_small_for_a_dquot_are_refused() {
        assert!(record_address(0, 65534).is_err());
        assert!(record_address(135, 65534).is_err());
    }

    fn seal(record: &mut [u8]) {
        let crc = crate::superblock::crc32c_with_zeroed_crc(record, DQ_CRC);
        record[DQ_CRC..DQ_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    }

    // Sparse pages keep the formatter's 320 MiB device and 64 MiB log
    // inexpensive without fixtures, a VM, or host filesystem tools.
    #[derive(Default)]
    struct MemoryDevice(std::sync::Mutex<BTreeMap<u64, Vec<u8>>>);

    impl fs_core::BlockRead for MemoryDevice {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let pages = self.0.lock().unwrap();
            let mut done = 0;
            while done < buf.len() {
                let at = offset as usize + done;
                let within = at % 4096;
                let count = (4096 - within).min(buf.len() - done);
                if let Some(page) = pages.get(&(at as u64 / 4096)) {
                    buf[done..done + count].copy_from_slice(&page[within..within + count]);
                } else {
                    buf[done..done + count].fill(0);
                }
                done += count;
            }
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            320 * 1024 * 1024
        }
    }

    impl fs_core::BlockDevice for MemoryDevice {
        fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
            let mut pages = self.0.lock().unwrap();
            let mut done = 0;
            while done < buf.len() {
                let at = offset as usize + done;
                let within = at % 4096;
                let count = (4096 - within).min(buf.len() - done);
                let bytes = &buf[done..done + count];
                let page_id = at as u64 / 4096;
                if bytes.iter().any(|byte| *byte != 0) || pages.contains_key(&page_id) {
                    pages.entry(page_id).or_insert_with(|| vec![0; 4096])[within..within + count]
                        .copy_from_slice(bytes);
                }
                done += count;
            }
            Ok(())
        }

        fn is_writable(&self) -> bool {
            true
        }
    }

    fn quota_filesystem(blocksize: u32) -> (Filesystem, std::sync::Arc<MemoryDevice>, u64, u64) {
        let dev = std::sync::Arc::new(MemoryDevice::default());
        crate::mkfs::format(
            dev.as_ref(),
            &crate::mkfs::Options {
                block_size: blocksize,
                ..Default::default()
            },
        )
        .unwrap();
        let mut fs = Filesystem::mount_rw(dev.clone()).unwrap();
        let root = fs.sb.rootino;
        let qino = fs.create_file(root, b"quota", 0o600).unwrap().0;
        let mut records = vec![0; blocksize as usize];
        for id in 0..=1 {
            let record = &mut records[id * DQBLK_SIZE..(id + 1) * DQBLK_SIZE];
            record[0..2].copy_from_slice(&DQ_MAGIC.to_be_bytes());
            record[2] = DQ_VERSION;
            record[3] = DQ_USER;
            record[4..8].copy_from_slice(&(id as u32).to_be_bytes());
            if id == 1 {
                // Linux v6.1 xfs_qm_scall_setqlim converts a one-block
                // byte limit with XFS_B_TO_FSB; xfs_dquot_to_disk stores
                // that value directly in both hard and soft limits.
                record[DQ_BHARD..DQ_BHARD + 8].copy_from_slice(&1u64.to_be_bytes());
                record[16..24].copy_from_slice(&1u64.to_be_bytes());
                record[DQ_ICOUNT..DQ_ICOUNT + 8].copy_from_slice(&1u64.to_be_bytes());
            }
            seal(record);
        }
        fs.write_into_empty_file(qino, &records).unwrap();
        let victim = fs.create_file(root, b"victim", 0o600).unwrap().0;
        let (_, mut raw) = fs.read_inode_raw(victim).unwrap();
        let uid = crate::inode::offsets::UID;
        raw[uid..uid + 4].copy_from_slice(&1u32.to_be_bytes());
        fs.logged_inode(victim, &raw, &[]).unwrap();
        let (qfile, qraw) = fs.read_inode_raw(qino).unwrap();
        let qblock = fs.data_extents(&qfile, &qraw).unwrap()[0].startblock;
        fs.sb.uquotino = qino;
        fs.sb.qflags = 3;
        (fs, dev, victim, qblock)
    }

    fn block_usage(fs: &Filesystem, qblock: u64, id: usize) -> u64 {
        let block = fs.read_fsblock(qblock).unwrap();
        let at = id * DQBLK_SIZE + DQ_BCOUNT;
        u64::from_be_bytes(block[at..at + 8].try_into().unwrap())
    }

    #[test]
    fn one_filesystem_block_fits_the_linux_limit_and_truncate_returns_zero() {
        for blocksize in [4096, 2048, 1024] {
            let (fs, dev, victim, qblock) = quota_filesystem(blocksize);
            let payload = vec![0x5a; blocksize as usize];
            fs.write_into_empty_file(victim, &payload)
                .expect("one filesystem block must fit the Linux one-block limit");
            assert_eq!(block_usage(&fs, qblock, 1), 1);
            assert_eq!(fs.read_path("/victim").unwrap(), payload);
            fs.truncate_to_zero(victim).unwrap();
            assert_eq!(block_usage(&fs, qblock, 1), 0);
            let before = dev.0.lock().unwrap().clone();
            let inode_before = fs.read_inode_raw(victim).unwrap().1;
            let free_before = fs.free_extents(0).unwrap();
            let dirty_before = fs.dirty_bytes();
            let error = fs
                .write_into_empty_file(victim, &vec![0x6b; 2 * blocksize as usize])
                .expect_err("two filesystem blocks must exceed the one-block limit");
            assert!(error.to_string().contains("hard limit"), "{error}");
            assert_eq!(
                *dev.0.lock().unwrap(),
                before,
                "refusal changed data or journal"
            );
            assert_eq!(fs.read_inode_raw(victim).unwrap().1, inode_before);
            assert_eq!(fs.free_extents(0).unwrap(), free_before);
            assert_eq!(fs.dirty_bytes(), dirty_before);
            assert_eq!(block_usage(&fs, qblock, 1), 0);
        }
    }

    #[test]
    fn directory_growth_accounts_filesystem_blocks_at_every_geometry() {
        for blocksize in [4096, 2048, 1024] {
            let (fs, _, _, qblock) = quota_filesystem(blocksize);
            let root = fs.sb.rootino;
            let initial = fs.read_inode(root).unwrap().nblocks;
            // Promotion allocates one 4 KiB directory block; subsequent
            // inserts into that block must not charge it again.
            for index in 0..16 {
                let name = format!("entry-{index:03}-{}", "x".repeat(80));
                fs.create_file(root, name.as_bytes(), 0o600).unwrap();
                let blocks = fs.read_inode(root).unwrap().nblocks;
                assert_eq!(block_usage(&fs, qblock, 0), blocks - initial);
            }
            assert_eq!(
                fs.read_inode(root).unwrap().nblocks,
                4096 / u64::from(blocksize)
            );
        }
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
