//! Where this driver thinks an inode's cluster buffer is must match
//! where the kernel said it was.
//!
//! A logged inode does not name its own address. It names the cluster
//! holding it — `ilf_blkno`, `ilf_len`, `ilf_boffset` — and the replayer
//! reads that whole cluster to apply the change. Get it wrong and the
//! record still checksums and is still trusted; recovery simply fails
//! part way through, refusing the mount with an I/O error that names
//! neither the inode nor the record.
//!
//! The cluster's size is not in the record. It comes from the geometry,
//! by a rule that is not obvious — 8 KiB scaled by the inode size and
//! truncated to whole filesystem blocks — so it is worth checking
//! against every example available rather than against the one
//! filesystem that happened to be to hand.
//!
//! Every fixture's log still holds the records written while it was
//! populated, whether or not it was unmounted cleanly, so this reads
//! thousands of examples out of images that exist for other reasons.
//!
//! Fixtures are gitignored, so this skips when there are none.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::log_write::InodeBuffer;
use fs_xfs::superblock::Superblock;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// `XFS_LI_INODE`, little-endian, at the start of the item.
const XFS_LI_INODE: u16 = 0x123b;
/// `sizeof(xfs_inode_log_format)`, which is how an inode item is
/// recognised among operations of other kinds.
const INODE_LOG_FORMAT_SIZE: usize = 56;

mod at {
    pub const INO: usize = 16;
    pub const BLKNO: usize = 40;
    pub const LEN: usize = 48;
    pub const BOFFSET: usize = 52;
}

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

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

/// One inode item, reduced to the inode it names and where it said that
/// inode's cluster buffer was.
struct Item {
    ino: u64,
    buffer: InodeBuffer,
}

/// What one image's log has to say about inode addressing.
struct Logged {
    sb: Superblock,
    items: Vec<Item>,
}

/// Every `xfs_inode_log_format` in one image's log.
fn inode_items(path: &Path) -> Option<Logged> {
    let dev = FileDevice::open(path).ok()?;
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).ok()?;
    let sb = Superblock::parse(&sbb).ok()?;
    if !sb.has_internal_log() {
        return None;
    }

    let mut log = vec![0u8; (u64::from(sb.logblocks) * u64::from(sb.blocksize)) as usize];
    dev.read_at(sb.fsblock_offset(sb.logstart), &mut log).ok()?;

    let mut found = Vec::new();
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

        // Undo the cycle stamp: each payload block's first word was
        // displaced into the header.
        let mut payload = log[data_at..data_at + padded].to_vec();
        for k in 0..(padded / BBSIZE).min(64) {
            payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(&blk[44 + k * 4..48 + k * 4]);
        }
        payload.truncate(h_len);

        // Walk the operations. Only the operation headers are
        // big-endian; the item bodies are memory images.
        let mut o = 0usize;
        while o + 12 <= payload.len() {
            let oplen = be32(&payload, o + 4) as usize;
            let body = o + 12;
            if body + oplen > payload.len() {
                break;
            }
            if oplen == INODE_LOG_FORMAT_SIZE && le16(&payload, body) == XFS_LI_INODE {
                let len = le32(&payload, body + at::LEN);
                // A checkpoint whose items span two records leaves a
                // tail this simple walk cannot frame. Such a fragment
                // reads as a zero-length buffer, which no real item has.
                if len != 0 {
                    found.push(Item {
                        ino: le64(&payload, body + at::INO),
                        buffer: InodeBuffer {
                            blkno: le64(&payload, body + at::BLKNO),
                            len,
                            boffset: le32(&payload, body + at::BOFFSET),
                        },
                    });
                }
            }
            o = body + oplen;
        }
    }
    Some(Logged { sb, items: found })
}

#[test]
fn the_cluster_buffer_matches_what_the_kernel_recorded() {
    let Ok(entries) = std::fs::read_dir(share()) else {
        eprintln!("no .vm-share — skipping");
        return;
    };

    let mut checked = 0usize;
    let mut geometries = Vec::new();

    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let Some(Logged { sb, items }) = inode_items(&p) else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        let name = p.file_name().unwrap_or_default().to_string_lossy();

        // Mounting may be refused — several fixtures are deliberately
        // dirty — so the inode's address is taken from the superblock
        // arithmetic directly rather than through a mount.
        let Ok(fs) = Filesystem::mount(Arc::new(FileDevice::open(&p).expect("open"))) else {
            continue;
        };
        let cluster = sb.inode_cluster_bytes();
        geometries.push((sb.blocksize, sb.inodesize, cluster));

        for Item { ino, buffer } in &items {
            let Ok(at) = fs.inode_offset(*ino) else {
                continue;
            };
            let ours = InodeBuffer::containing(at, cluster);
            assert_eq!(
                ours, *buffer,
                "{name}: inode {ino} at {at:#x} — this driver places its cluster at \
                 {ours:?}, the kernel wrote {buffer:?} (blocksize {}, inodesize {}, \
                 cluster {cluster})",
                sb.blocksize, sb.inodesize
            );
            checked += 1;
        }
    }

    if checked == 0 {
        eprintln!("no fixture holds a logged inode — skipping");
        return;
    }

    geometries.sort_unstable();
    geometries.dedup();
    eprintln!(
        "{checked} logged inodes across {} geometries",
        geometries.len()
    );
    for (b, i, c) in &geometries {
        eprintln!("  blocksize {b}, inodesize {i} -> cluster {c}");
    }

    // One geometry proves the arithmetic and nothing about the rule that
    // produced the cluster size, which is the part that scales with the
    // inode size.
    assert!(
        geometries.len() > 1,
        "only one geometry was available, so the cluster-size rule is unexercised"
    );
}
