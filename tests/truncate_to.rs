//! A file is truncated to any length, the blocks past its new end freed,
//! and the bytes past it never seen again (#370).
//!
//! `Filesystem::truncate_to` cuts a file shorter or makes it longer in one
//! record: blocks wholly past the new end go back to free space, an extent
//! that straddles it keeps only its part inside the file, and the last
//! block's bytes past the end are zeroed so that a later grow shows zeros.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device. Fragmented and B+tree files,
//! shared extents, and the kernel's reading of every case are
//! `tests/truncate_to_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

struct Sparse(Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]))[within..within + n]
                .copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn mounted() -> Filesystem {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn free_blocks(fs: &Filesystem) -> u64 {
    (0..fs.superblock().agcount)
        .map(|ag| u64::from(fs.read_agf(ag).expect("agf").freeblks))
        .sum()
}

fn read(fs: &Filesystem, ino: u64) -> Vec<u8> {
    let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
    fs.read_file(&inode, &raw).expect("read")
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(7) | 1).collect()
}

/// A file of ten blocks of data, and the block size.
fn ten_blocks(fs: &Filesystem) -> (u64, Vec<u8>, u64) {
    let bs = u64::from(fs.superblock().blocksize);
    let root = fs.superblock().rootino;
    let (ino, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    let data = pattern(10 * bs as usize);
    fs.write_into_empty_file(ino, &data).expect("fill");
    (ino, data, bs)
}

#[test]
fn a_cut_at_a_block_boundary_frees_the_blocks_past_it() {
    let fs = mounted();
    let (ino, data, bs) = ten_blocks(&fs);
    let free = free_blocks(&fs);
    fs.truncate_to(ino, 4 * bs)
        .expect("truncate to four blocks");
    let inode = fs.read_inode(ino).expect("inode");
    assert_eq!((inode.size, inode.nblocks), (4 * bs, 4));
    assert_eq!(read(&fs, ino), data[..4 * bs as usize]);
    assert!(free_blocks(&fs) >= free + 6, "six blocks did not come back");
}

#[test]
fn a_cut_inside_a_block_hides_the_rest_of_it_for_good() {
    let fs = mounted();
    let (ino, data, bs) = ten_blocks(&fs);
    let cut = 3 * bs + 100;
    fs.truncate_to(ino, cut)
        .expect("truncate inside block three");
    let inode = fs.read_inode(ino).expect("inode");
    assert_eq!(
        (inode.size, inode.nblocks),
        (cut, 4),
        "block three is the last kept"
    );
    assert_eq!(read(&fs, ino), data[..cut as usize]);
    // Grown back over what was cut: zeros, not the old bytes.
    fs.truncate_to(ino, 6 * bs).expect("grow");
    let mut want = data[..cut as usize].to_vec();
    want.resize(6 * bs as usize, 0);
    assert_eq!(
        read(&fs, ino),
        want,
        "the cut bytes came back when the file grew"
    );
    assert_eq!(
        fs.read_inode(ino).expect("inode").nblocks,
        4,
        "a grow allocated"
    );
}

#[test]
fn truncating_to_zero_and_growing_from_nothing() {
    let fs = mounted();
    let (ino, _, bs) = ten_blocks(&fs);
    let free = free_blocks(&fs);
    fs.truncate_to(ino, 0).expect("truncate to zero");
    let inode = fs.read_inode(ino).expect("inode");
    assert_eq!((inode.size, inode.nblocks, inode.nextents), (0, 0, 0));
    assert!(free_blocks(&fs) >= free + 10);
    fs.truncate_to(ino, 3 * bs + 5).expect("grow from zero");
    assert_eq!(read(&fs, ino), vec![0u8; 3 * bs as usize + 5]);
}

#[test]
fn the_same_size_changes_nothing() {
    let fs = mounted();
    let (ino, data, _) = ten_blocks(&fs);
    assert_eq!(fs.truncate_to(ino, data.len() as u64).expect("no-op"), 0);
    assert_eq!(read(&fs, ino), data);
}
