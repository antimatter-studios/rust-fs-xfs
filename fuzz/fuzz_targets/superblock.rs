#![no_main]
//! The superblock is the one structure read before anything is known,
//! and every number the rest of the driver divides and multiplies by
//! comes out of it -- block size, inode size, AG size, log geometry.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::Superblock::parse(&fs_xfs_fuzz::block(data, fs_xfs_fuzz::SECTOR));
});
