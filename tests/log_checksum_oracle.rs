//! The log record checksum, checked against records the kernel wrote.
//!
//! A log writer cannot be trusted without this. The kernel treats a
//! record whose checksum does not verify as a torn write: it truncates
//! the log head back and discards that record and everything after it.
//! So a writer that computes it wrongly does not corrupt anything — it
//! silently does nothing, which is a failure no amount of reading our
//! own output would reveal.
//!
//! What the checksum covers was not obvious and is worth stating, since
//! ten plausible layouts were tried against real records and every one
//! was wrong:
//!
//! - the header contributes `sizeof(xlog_rec_header)` — **328 bytes**,
//!   the fields padded to the 8-byte alignment its `u64` members impose.
//!   Not the 512-byte basic block the header occupies on disk; the
//!   remaining 184 bytes are padding the checksum never sees.
//! - the data is `h_len` bytes **as written**, with the cycle stamp
//!   already applied. The checksum is taken after packing, not before.
//!
//! Fixtures are gitignored, so this skips on a fresh clone.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{record_checksum, BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::superblock::Superblock;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every checksummed record in every fixture must verify.
#[test]
fn every_record_the_kernel_wrote_verifies() {
    let Ok(entries) = std::fs::read_dir(share()) else {
        eprintln!("no .vm-share — skipping");
        return;
    };

    let mut images = 0usize;
    let mut records = 0usize;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let Ok(dev) = FileDevice::open(&p) else {
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

        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut here = 0usize;
        for i in 0..log.len() / BBSIZE {
            let blk = &log[i * BBSIZE..(i + 1) * BBSIZE];
            if u32::from_be_bytes(blk[0..4].try_into().unwrap()) != XLOG_HEADER_MAGIC {
                continue;
            }
            // Another filesystem's stale record, or bytes that merely
            // look like a header.
            if blk[304..320] != sb.uuid[..] {
                continue;
            }
            let stored = u32::from_le_bytes(blk[32..36].try_into().unwrap());
            // mkfs writes its initial unmount record with no checksum.
            // Zero means "not computed" there; it is not a value to
            // reproduce.
            if stored == 0 {
                continue;
            }
            let h_len = u32::from_be_bytes(blk[12..16].try_into().unwrap()) as usize;
            let data_at = (i + 1) * BBSIZE;
            let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
            if data_at + padded > log.len() {
                continue;
            }

            let got = record_checksum(blk, &log[data_at..data_at + h_len]);
            assert_eq!(
                got, stored,
                "{name}: record at basic block {i} (h_len {h_len}) checksums to {got:#010x}, \
                 the kernel stored {stored:#010x}"
            );
            here += 1;
        }
        if here > 0 {
            eprintln!("{name}: {here} records verified");
            images += 1;
            records += here;
        }
    }

    if records == 0 {
        eprintln!("no fixtures with checksummed log records — skipping");
        return;
    }
    eprintln!("{records} records across {images} filesystems");
}

/// The header contributes its struct size, not its basic block.
///
/// This is the whole finding, and the one thing a future change could
/// break without any other test noticing — using 512 here verifies
/// nothing and looks entirely reasonable.
#[test]
fn the_header_contributes_its_struct_size_not_its_block() {
    // 328, not the 512 of the block it sits in. The 184-byte difference
    // is padding the checksum does not cover, and using 512 here would
    // verify nothing while looking entirely reasonable.
    assert_eq!(fs_xfs::log::XLOG_REC_HEADER_SIZE, 328);
}
