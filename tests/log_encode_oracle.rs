//! The record encoder, checked by re-encoding what the kernel wrote.
//!
//! An encoder verified against its own output proves nothing. This takes
//! records the kernel produced, extracts the inputs they were built
//! from, re-encodes them, and requires the result to be byte-identical.
//!
//! It is worth being strict about this. A record whose checksum does not
//! verify is discarded by the kernel as a torn write, so a wrong encoder
//! does not corrupt anything — it silently does nothing. Byte equality
//! against a real record is the only cheap way to notice.
//!
//! Fixtures are gitignored and generated. They are built on every run
//! now — `chore fixtures` makes the whole set — so a clone with none of
//! them fails here rather than passing having re-encoded nothing.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::log_write::{encode_record, Placement};
use fs_xfs::superblock::Superblock;

mod common;
use common::fixtures_matching;

/// Re-encode every checksummed record and require byte equality.
#[test]
fn re_encoding_reproduces_records_byte_for_byte() {
    let mut checked = 0usize;
    for p in fixtures_matching("", ".img") {
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

        for i in 0..log.len() / BBSIZE {
            let blk = &log[i * BBSIZE..(i + 1) * BBSIZE];
            if u32::from_be_bytes(blk[0..4].try_into().unwrap()) != XLOG_HEADER_MAGIC {
                continue;
            }
            if blk[304..320] != sb.uuid[..] {
                continue;
            }
            let stored_crc = u32::from_le_bytes(blk[32..36].try_into().unwrap());
            if stored_crc == 0 {
                continue; // mkfs's unmount record carries no checksum
            }
            let h_len = u32::from_be_bytes(blk[12..16].try_into().unwrap()) as usize;
            let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
            let data_at = (i + 1) * BBSIZE;
            if data_at + padded > log.len() {
                continue;
            }

            // Recover the payload as it was before stamping: each block's
            // first word comes back from h_cycle_data.
            let mut payload = log[data_at..data_at + padded].to_vec();
            for k in 0..(padded / BBSIZE).min(64) {
                let original = &blk[44 + k * 4..48 + k * 4];
                payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(original);
            }
            payload.truncate(h_len);

            let placement = Placement {
                block: u32::from_be_bytes(blk[16 + 4..24].try_into().unwrap()),
                cycle: u32::from_be_bytes(blk[4..8].try_into().unwrap()),
                prev_block: u32::from_be_bytes(blk[36..40].try_into().unwrap()),
                tail_lsn: u64::from_be_bytes(blk[24..32].try_into().unwrap()),
                uuid: sb.uuid,
                iclog_size: u32::from_be_bytes(blk[320..324].try_into().unwrap()),
            };
            let num_logops = u32::from_be_bytes(blk[40..44].try_into().unwrap());

            let ours = encode_record(&placement, num_logops, &payload);
            let theirs = &log[i * BBSIZE..data_at + padded];
            assert_eq!(
                ours.len(),
                theirs.len(),
                "{name}: record at block {i} re-encodes to {} bytes, the kernel wrote {}",
                ours.len(),
                theirs.len()
            );
            if ours != theirs {
                let first = (0..ours.len()).find(|&k| ours[k] != theirs[k]).unwrap();
                panic!(
                    "{name}: record at block {i} differs from the kernel's at byte {first} \
                     (ours {:#04x}, theirs {:#04x}); h_len {h_len}, num_logops {num_logops}",
                    ours[first], theirs[first]
                );
            }
            checked += 1;
        }
    }

    // THE FLOOR IS THE ONLY GUARD LEFT. Every assertion above is inside
    // the loop, so a run that found no checksummed record anywhere would
    // otherwise report a green test that compared nothing. The fixtures
    // always carry records the kernel wrote, so none of them having one
    // is a fixture build that went wrong, not a run with nothing to do.
    assert!(
        checked > 0,
        "no checksummed log record in any .vm-share image: the fixtures are built \
         by one mkfs.xfs in the harness guest and every one of them has records \
         the kernel wrote. `chore fixtures` rebuilds them."
    );
    eprintln!("{checked} records re-encoded byte-for-byte");
}
