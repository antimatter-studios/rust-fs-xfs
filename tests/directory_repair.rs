//! A directory's redundant metadata is rebuilt from its entries by the
//! planner's directory rule, and nothing else changes (#394).
//!
//! The volume is this crate's `mkfs` in memory: a write through the driver
//! leaves a log that needs replay, which a repair refuses, so the damage
//! is made directly in the root directory's short-form header. What the
//! kernel and `xfs_repair -n` make of every kind of directory repair is
//! `tests/cli_directory_repair_kernel.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::repair::{apply, plan, Exclusive, Status};
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

fn volume() -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    dev
}

fn mount(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount")
}

fn image(dev: &Arc<Sparse>) -> BTreeMap<u64, Box<[u8; SECTOR]>> {
    dev.0.lock().unwrap().clone()
}

#[test]
fn a_root_whose_parent_is_wrong_is_given_itself_again() {
    let dev = volume();
    let clean = image(&dev);
    let fs = mount(&dev);
    let root = fs.superblock().rootino;
    let at = fs.inode_offset(root).unwrap();
    let (inode, mut raw) = fs.read_inode_raw(root).unwrap();
    let (start, _) = inode.data_fork_range(usize::from(fs.superblock().inodesize));
    // The short-form header's parent, four bytes after the counts.
    raw[start + 2..start + 6].copy_from_slice(&0u32.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut raw, fs_xfs::inode::offsets::CRC);
    dev.write_at(at, &raw).unwrap();
    assert!(image(&dev) != clean);

    let fs = mount(&dev);
    let access = Exclusive::asserted_by_caller();
    let planned = plan(&fs, &access);
    assert_eq!(planned.status, Status::Ready, "{:?}", planned.refusals);
    assert!(!planned.changes.is_empty(), "nothing was proposed");
    apply(&planned, dev.as_ref(), 0, &access).expect("apply");
    assert!(image(&dev) == clean, "not the bytes from before the damage");
    let again = plan(&mount(&dev), &access);
    assert!(again.changes.is_empty());
}

#[test]
fn a_clean_volume_is_left_alone() {
    let dev = volume();
    let fs = mount(&dev);
    let access = Exclusive::asserted_by_caller();
    let planned = plan(&fs, &access);
    assert_eq!(planned.status, Status::Ready);
    assert!(planned.changes.is_empty(), "{:?}", planned.changes);
}
