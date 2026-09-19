#![no_main]
//! The AG inode header, including the unlinked-inode hash buckets that
//! a mount walks before anything else has been validated.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::ag::Agi::parse(
        &fs_xfs_fuzz::block(data, fs_xfs_fuzz::SECTOR),
        fs_xfs_fuzz::superblock(),
        0,
    );
});
