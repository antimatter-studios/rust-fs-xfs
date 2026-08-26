//! Dump every operation in every log record, so a transaction's shape
//! can be read off rather than guessed.
//!
//! `cargo run --example dump_ops -- <image> [--since <block>]`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::superblock::Superblock;

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn le64(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .expect("usage: dump_ops <image> [--since <block>]");
    let since: u64 = match (args.next().as_deref(), args.next()) {
        (Some("--since"), Some(b)) => b.parse().expect("a block number"),
        _ => 0,
    };

    let dev = FileDevice::open(&path).expect("open");
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).expect("read");
    let sb = Superblock::parse(&sbb).expect("superblock");
    let mut log = vec![0u8; (u64::from(sb.logblocks) * u64::from(sb.blocksize)) as usize];
    dev.read_at(sb.fsblock_offset(sb.logstart), &mut log)
        .expect("read log");

    for i in 0..log.len() / BBSIZE {
        let blk = &log[i * BBSIZE..(i + 1) * BBSIZE];
        if be32(blk, 0) != XLOG_HEADER_MAGIC || blk[304..320] != sb.uuid[..] {
            continue;
        }
        if (i as u64) < since {
            continue;
        }
        let h_len = be32(blk, 12) as usize;
        let logops = be32(blk, 40);
        let lsn = u64::from_be_bytes(blk[16..24].try_into().unwrap());
        // A cycle of zero is not a record. mkfs leaves the ring stamped
        // in a way that satisfies the magic and the UUID, and those
        // blocks carry no length and no operations.
        if h_len == 0 || logops == 0 || (lsn >> 32) == 0 {
            continue;
        }
        let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
        let data_at = (i + 1) * BBSIZE;
        if data_at + padded > log.len() {
            continue;
        }
        let mut payload = log[data_at..data_at + padded].to_vec();
        for k in 0..(padded / BBSIZE).min(64) {
            payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(&blk[44 + k * 4..48 + k * 4]);
        }
        payload.truncate(h_len);

        println!("record @{i}: lsn {lsn:#x}, h_len {h_len}, {logops} ops");
        let mut at = 0usize;
        let mut n = 0;
        // `h_num_logops` is how many there are. Walking to the end of
        // the payload instead reads the record's zero padding as an
        // endless run of empty operations.
        while n < logops && at + 12 <= payload.len() {
            let tid = be32(&payload, at);
            let oplen = be32(&payload, at + 4) as usize;
            let flags = payload[at + 9];
            let body = at + 12;
            if body + oplen > payload.len() {
                println!("    op {n}: len {oplen} runs past the record");
                break;
            }
            let b = &payload[body..body + oplen];
            let mut note = String::new();
            if oplen == 16 && le32(b, 0) == 0x5452_414e {
                note = format!(
                    "TRANS type {:#x} tid {:#x} item-ops {}",
                    le32(b, 4),
                    le32(b, 8),
                    le32(b, 12)
                );
            } else if oplen == 56 && le16(b, 0) == 0x123b {
                note = format!(
                    "INODE ino {} size {} fields {:#06x} dsize {} asize {} blkno {} len {} boff {}",
                    le64(b, 16),
                    le16(b, 2),
                    le32(b, 4),
                    le16(b, 10),
                    le16(b, 8),
                    le64(b, 40),
                    le32(b, 48),
                    le32(b, 52)
                );
            } else if oplen >= 20 && le16(b, 0) == 0x123c {
                let flags = le16(b, 4);
                note = format!(
                    "BUF size {} type {} flags {:#05x} len {} blkno {} map_size {}",
                    le16(b, 2),
                    flags >> 11,
                    flags & 0x7ff,
                    le16(b, 6),
                    le64(b, 8),
                    le32(b, 16)
                );
            } else if oplen == 176 || oplen == 96 {
                note = format!("core? di_mode {:#o}", le16(b, 2));
            } else if oplen > 0 && oplen <= 64 {
                note = format!("bytes {:02x?}", &b[..oplen.min(48)]);
            }
            println!("    op {n}: tid {tid:#x} len {oplen:>4} flags {flags:#04x}  {note}",);
            at = body + oplen;
            n += 1;
        }
    }
}
