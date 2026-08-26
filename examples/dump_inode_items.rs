//! Dump every `xfs_inode_log_format` the kernel wrote, so the three
//! fields that address the inode's buffer can be read off rather than
//! guessed.
//!
//! `cargo run --example dump_inode_items -- <image>...`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::superblock::Superblock;

/// `XFS_LI_INODE`, little-endian in the item's first two bytes.
const XFS_LI_INODE: u16 = 0x123b;

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}
fn le64(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().unwrap())
}
fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn main() {
    for path in std::env::args().skip(1) {
        let Ok(dev) = FileDevice::open(&path) else {
            continue;
        };
        let mut sbb = vec![0u8; 4096];
        if dev.read_at(0, &mut sbb).is_err() {
            continue;
        }
        let Ok(sb) = Superblock::parse(&sbb) else {
            continue;
        };
        if !sb.has_internal_log() {
            continue;
        }
        let at = sb.fsblock_offset(sb.logstart);
        let len = u64::from(sb.logblocks) * u64::from(sb.blocksize);
        let mut log = vec![0u8; len as usize];
        if dev.read_at(at, &mut log).is_err() {
            continue;
        }

        println!(
            "== {path}: blocksize {} inodesize {} agblocks {} sectsize {}",
            sb.blocksize, sb.inodesize, sb.agblocks, sb.sectsize
        );

        for i in 0..log.len() / BBSIZE {
            let blk = &log[i * BBSIZE..(i + 1) * BBSIZE];
            if be32(blk, 0) != XLOG_HEADER_MAGIC || blk[304..320] != sb.uuid[..] {
                continue;
            }
            let h_len = be32(blk, 12) as usize;
            let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
            let data_at = (i + 1) * BBSIZE;
            if h_len == 0 || data_at + padded > log.len() {
                continue;
            }
            let mut payload = log[data_at..data_at + padded].to_vec();
            for k in 0..(padded / BBSIZE).min(64) {
                payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(&blk[44 + k * 4..48 + k * 4]);
            }
            payload.truncate(h_len);

            // Walk the operations. Each has a 12-byte header; the length
            // is big-endian even though the item bodies are not.
            let mut at = 0usize;
            while at + 12 <= payload.len() {
                let oplen = be32(&payload, at + 4) as usize;
                let body = at + 12;
                if body + oplen > payload.len() {
                    break;
                }
                if oplen == 56 && le16(&payload, body) == XFS_LI_INODE {
                    let ino = le64(&payload, body + 16);
                    let blkno = le64(&payload, body + 40) as i64;
                    let blen = le32(&payload, body + 48) as i32;
                    let boff = le32(&payload, body + 52) as i32;
                    let (ag, agbno, off) = sb.split_ino(ino);
                    let inode_at = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize)
                        + u64::from(agbno) * u64::from(sb.blocksize)
                        + u64::from(off) * u64::from(sb.inodesize);
                    println!(
                        "  rec@{i:6} ino {ino:>8} fields {:#06x} size {} | blkno {blkno:>8} \
                         len {blen:>3} boffset {boff:>6} | buf@{:#012x} inode@{inode_at:#012x} \
                         delta {}",
                        le32(&payload, body + 4),
                        le16(&payload, body + 2),
                        blkno as u64 * BBSIZE as u64,
                        inode_at as i64 - (blkno * BBSIZE as i64),
                    );
                }
                at = body + oplen;
            }
        }
    }
}
