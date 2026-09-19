#![no_main]
//! The AG free-space header carries the btree roots and level counts the
//! allocator walks, and the free-block counters the accounting trusts.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::ag::Agf::parse(
        &fs_xfs_fuzz::block(data, fs_xfs_fuzz::SECTOR),
        fs_xfs_fuzz::superblock(),
        0,
    );
});
