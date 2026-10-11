//! A realtime section the superblock claims, and no realtime device to
//! hold it, does not decide how much memory a read or a check asks for
//! (#444).
//!
//! `sb_rblocks` is bounded by the realtime device's size only when one is
//! given. Without one it is any number, and two things sized themselves
//! by it: `read_file`'s ceiling on a whole-file read (the data and the
//! realtime section together), and the checker's realtime walk, which
//! kept one byte and one owner per realtime extent. A superblock whose
//! realtime fields agree with each other at a large size passes every
//! geometry check and asks for gigabytes, which aborts the process instead
//! of reporting damage.
//!
//! Memory is measured, not hoped about: this binary's allocator records the
//! largest single allocation, and every case keeps it under 64 MiB. A
//! zeroed allocation is lazy on Linux, so a test that only waited for an
//! abort could pass on one machine and kill the run on another.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::superblock::offsets as sbo;
use fs_xfs::Filesystem;
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

struct Measured;

static LARGEST: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Measured {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LARGEST.fetch_max(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        LARGEST.fetch_max(new, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new) }
    }
}

#[global_allocator]
static ALLOCATOR: Measured = Measured;

/// The ceiling on any one allocation a read or a check makes here.
const CEILING: usize = 64 << 20;

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

/// 2^28 realtime extents of one 4 KiB block: a terabyte of realtime
/// section, described consistently, on a volume with no realtime device.
const REXTENTS: u64 = 1 << 28;

/// This crate's `mkfs`, then every superblock copy given a large,
/// self-consistent realtime section.
fn volume() -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let sb = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>)
        .expect("mount")
        .superblock()
        .clone();
    let bs = u64::from(sb.blocksize);
    let rbmblocks = REXTENTS.div_ceil(bs * 8) as u32;
    for ag in 0..u64::from(sb.agcount) {
        let at = ag * u64::from(sb.agblocks) * bs;
        let mut s = vec![0u8; SECTOR];
        dev.read_at(at, &mut s).unwrap();
        s[sbo::RBLOCKS..sbo::RBLOCKS + 8].copy_from_slice(&REXTENTS.to_be_bytes());
        s[sbo::REXTENTS..sbo::REXTENTS + 8].copy_from_slice(&REXTENTS.to_be_bytes());
        s[sbo::REXTSIZE..sbo::REXTSIZE + 4].copy_from_slice(&1u32.to_be_bytes());
        s[sbo::RBMBLOCKS..sbo::RBMBLOCKS + 4].copy_from_slice(&rbmblocks.to_be_bytes());
        s[sbo::REXTSLOG] = 28;
        fs_xfs::group_write::restamp_crc(&mut s, sbo::CRC);
        dev.write_at(at, &s).unwrap();
    }
    dev
}

#[test]
fn checking_a_claimed_realtime_section_allocates_what_files_map_not_what_it_claims() {
    let dev = volume();
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
    assert_eq!(
        fs.superblock().rextents,
        REXTENTS,
        "the geometry was not set"
    );
    LARGEST.store(0, Ordering::Relaxed);
    let report = fs_xfs::check::check(&fs);
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        largest < CEILING,
        "checking asked for {largest} bytes at once, sized by the superblock's claim of \
         {REXTENTS} realtime extents: {:?}",
        report.findings.iter().map(|f| f.code).collect::<Vec<_>>()
    );
    assert!(
        !report.is_clean(),
        "a realtime section with no bitmap to match is damage"
    );
}

#[test]
fn a_whole_file_read_is_bounded_by_the_data_device_without_a_realtime_one() {
    let dev = volume();
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount");
    let sb = fs.superblock().clone();
    // The realtime bitmap inode, a regular file, claiming a size past the
    // data device and inside the claimed realtime section.
    let ino = sb.rbmino;
    let at = fs.inode_offset(ino).unwrap();
    let data_bytes = sb.dblocks * u64::from(sb.blocksize);
    drop(fs);
    let mut raw = vec![0u8; usize::from(sb.inodesize)];
    dev.read_at(at, &mut raw).unwrap();
    raw[56..64].copy_from_slice(&(data_bytes + (1 << 30)).to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut raw, fs_xfs::inode::offsets::CRC);
    dev.write_at(at, &raw).unwrap();

    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    LARGEST.store(0, Ordering::Relaxed);
    let read = fs.read_file(&inode, &raw);
    let largest = LARGEST.load(Ordering::Relaxed);
    assert!(
        read.is_err(),
        "a {}-byte read on a {data_bytes}-byte volume with no realtime device was allowed",
        inode.size
    );
    assert!(
        largest < CEILING,
        "the refused read still asked for {largest} bytes"
    );
}
