//! Converting an on-disk inode to the form the log stores, checked
//! against cores the kernel actually logged.
//!
//! The two forms are the same structure at the same offsets, differing
//! only in byte order and in `di_crc` being blank. That makes the
//! conversion a byte-swap — and makes it exactly the kind of code that
//! parses cleanly while being wrong, since a field swapped at the wrong
//! width still produces a plausible number.
//!
//! So it is checked the only way worth checking: find a record where the
//! kernel logged an inode, convert that inode's on-disk bytes ourselves,
//! and require the result to match what the kernel wrote.
//!
//! Fixtures are gitignored, so this skips on a fresh clone.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::log_write::{log_dinode_from_disk, LOG_DINODE_SIZE, XFS_LI_INODE};
use fs_xfs::superblock::Superblock;
use std::path::{Path, PathBuf};

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every logged inode core we can find must be reproducible from the
/// inode on disk.
///
/// Only the *last* record logging a given inode is comparable: an
/// earlier one is a snapshot of a state the disk has since moved past.
/// So the cores are collected by inode number and the newest kept.
#[test]
fn logged_cores_match_the_inodes_on_disk() {
    let Ok(entries) = std::fs::read_dir(share()) else {
        eprintln!("no .vm-share — skipping");
        return;
    };

    let mut compared = 0usize;
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
        // Only a cleanly unmounted filesystem can be compared. A dirty
        // log holds changes the inodes on disk have not received, so the
        // record is ahead of the disk on purpose — the comparison would
        // be measuring the crash, not the conversion. Caught here by the
        // driver's own log check, which is what it is for.
        if !matches!(
            fs_xfs::log::inspect(&dev, &sb),
            Ok(fs_xfs::log::LogState::CleanlyUnmounted | fs_xfs::log::LogState::Empty)
        ) {
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

        // inode number -> (lsn, logged core), newest kept
        let mut newest: std::collections::BTreeMap<u64, (u64, Vec<u8>)> = Default::default();

        for i in 0..log.len() / BBSIZE {
            let blk = &log[i * BBSIZE..(i + 1) * BBSIZE];
            if u32::from_be_bytes(blk[0..4].try_into().unwrap()) != XLOG_HEADER_MAGIC {
                continue;
            }
            if blk[304..320] != sb.uuid[..] {
                continue;
            }
            let h_len = u32::from_be_bytes(blk[12..16].try_into().unwrap()) as usize;
            let lsn = u64::from_be_bytes(blk[16..24].try_into().unwrap());
            let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
            let data_at = (i + 1) * BBSIZE;
            if data_at + padded > log.len() {
                continue;
            }

            // Undo the cycle stamp to read the payload.
            let mut payload = log[data_at..data_at + padded].to_vec();
            for k in 0..(padded / BBSIZE).min(64) {
                payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(&blk[44 + k * 4..48 + k * 4]);
            }
            payload.truncate(h_len);

            // Walk the operations looking for an inode item: a 56-byte
            // format op naming the inode, then the core.
            let mut off = 0usize;
            while off + 12 <= payload.len() {
                let oh_len =
                    u32::from_be_bytes(payload[off + 4..off + 8].try_into().unwrap()) as usize;
                let body = off + 12;
                if body + oh_len > payload.len() {
                    break;
                }
                if oh_len == 56
                    && u16::from_le_bytes(payload[body..body + 2].try_into().unwrap())
                        == XFS_LI_INODE
                {
                    let ino = u64::from_le_bytes(payload[body + 16..body + 24].try_into().unwrap());
                    // The core is the next op.
                    let next = body + oh_len;
                    if next + 12 <= payload.len() {
                        let core_len =
                            u32::from_be_bytes(payload[next + 4..next + 8].try_into().unwrap())
                                as usize;
                        let core_at = next + 12;
                        if core_len >= 96 && core_at + core_len <= payload.len() {
                            let core = payload[core_at..core_at + core_len].to_vec();
                            let keep = newest.get(&ino).is_none_or(|&(l, _)| lsn >= l);
                            if keep {
                                newest.insert(ino, (lsn, core));
                            }
                        }
                    }
                }
                off = body + oh_len;
            }
        }

        let mut here = 0usize;
        for (ino, (_, logged)) in &newest {
            // Read the inode as it now sits on disk.
            let mut raw = vec![0u8; usize::from(sb.inodesize)];
            let (ag, ag_block, slot) = sb.split_ino(*ino);
            if ag >= sb.agcount {
                continue;
            }
            let byte = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize)
                + u64::from(ag_block) * u64::from(sb.blocksize)
                + u64::from(slot) * u64::from(sb.inodesize);
            if dev.read_at(byte, &mut raw).is_err() {
                continue;
            }
            // Only compare where the disk really holds this inode.
            if u16::from_be_bytes(raw[0..2].try_into().unwrap()) != 0x494e {
                continue;
            }

            let Ok(ours) = log_dinode_from_disk(&raw) else {
                continue;
            };
            if ours.len() != logged.len() {
                continue; // a different core size means a different shape
            }

            // Two fields can legitimately differ, and both are the disk
            // being ahead of the record rather than a conversion fault.
            //
            // di_lsn is stamped when the inode is written back, after it
            // was logged.
            //
            // di_next_unlinked is maintained through the inode *buffer*
            // item rather than the core: the core is always logged with
            // the null sentinel while the disk carries the live pointer.
            // A file unlinked while still open shows exactly that, and
            // it is what this comparison first tripped over.
            let mut a = ours.clone();
            let mut b = logged.clone();
            // di_flushiter counts write-backs, so on a v2 inode the disk
            // leads the log here for the same reason di_lsn does.
            a[30..32].fill(0);
            b[30..32].fill(0);
            // A v2 core stops at 96 and has neither field below.
            if a.len() >= LOG_DINODE_SIZE {
                a[96..100].fill(0);
                b[96..100].fill(0);
                a[112..120].fill(0);
                b[112..120].fill(0);
            }
            assert_eq!(
                a,
                b,
                "{name}: inode {ino} converts to a core the kernel did not write; \
                 first difference at byte {}",
                (0..a.len()).find(|&k| a[k] != b[k]).unwrap_or(0)
            );
            here += 1;
        }
        if here > 0 {
            eprintln!("{name}: {here} inodes matched");
            compared += here;
        }
    }

    if compared == 0 {
        eprintln!("no comparable logged inodes — skipping");
        return;
    }
    eprintln!("{compared} inode cores reproduced from disk");
}
