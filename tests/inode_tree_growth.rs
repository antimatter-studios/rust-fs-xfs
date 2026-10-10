//! A group's inode B+tree grows past one block, and the creates after it
//! succeed (#423).
//!
//! On 1 KiB blocks an inode-tree leaf holds 60 chunk records, so a group
//! outgrows one leaf at its 61st chunk of 64 inodes. Creating files there
//! used to fail at that point, reading back a root block whose level the
//! group's header did not record.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device. What the kernel and
//! `xfs_repair` make of the grown tree is `tests/inode_tree_growth_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 400 * 1024 * 1024;
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
    let options = fs_xfs::mkfs::Options {
        block_size: 1024,
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

/// Files in directories of a hundred, each one block, until `n` are made.
fn fill(fs: &Filesystem, n: usize) {
    let root = fs.superblock().rootino;
    for d in 0..n.div_ceil(100) {
        let (dir, _) = fs
            .create_directory(root, format!("d{d}").as_bytes(), 0o040755)
            .expect("mkdir");
        for i in 0..100.min(n - d * 100) {
            fs.create_file(dir, format!("f{i}").as_bytes(), 0o100644)
                .unwrap_or_else(|e| panic!("dir {d} file {i}: {e:?}"));
        }
    }
}

#[test]
fn creates_go_on_past_one_block_of_inode_tree() {
    let fs = mounted();
    fill(&fs, 10_000);
    let root = fs.superblock().rootino;
    let (root_inode, root_raw) = fs.read_inode_raw(root).expect("root");
    let d0 = fs.lookup(&root_inode, &root_raw, b"d0").expect("d0");
    assert!(d0.is_dir());
}

/// A create that takes an inode from a chunk the group already has takes
/// no blocks, and so has no business reading the group's free space: one
/// with a damaged free-space record still succeeds (#423), as
/// `tests/free_space_record_oracle.rs` requires of a kernel-made volume.
#[test]
fn a_create_that_takes_no_blocks_does_not_read_free_space() {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    let options = fs_xfs::mkfs::Options {
        block_size: 1024,
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    // Group 0's first free-space record moved onto the group's headers.
    let (bs, sect) = {
        let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount");
        (
            u64::from(fs.superblock().blocksize),
            u64::from(fs.superblock().sectsize),
        )
    };
    let mut agf = vec![0u8; sect as usize];
    dev.read_at(sect, &mut agf).unwrap();
    let at = fs_xfs::ag::offsets::agf::ROOTS;
    let bno_root = u64::from(u32::from_be_bytes(agf[at..at + 4].try_into().unwrap()));
    let mut block = vec![0u8; bs as usize];
    dev.read_at(bno_root * bs, &mut block).unwrap();
    let record = fs_xfs::ag_btree::V5_HEADER_LEN;
    block[record..record + 4].copy_from_slice(&0u32.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut block, fs_xfs::ag_btree::offsets::CRC);
    dev.write_at(bno_root * bs, &block).unwrap();

    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
    let root = fs.superblock().rootino;
    fs.create_file(root, b"f", 0o100644)
        .expect("the create takes an inode, not blocks");
}
