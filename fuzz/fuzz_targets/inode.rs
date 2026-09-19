#![no_main]
//! The format byte selects the fork layout, so one inode is really
//! several decoders behind one entry point -- local, extents and btree
//! each read the fork area differently.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::inode::Inode::parse(
        &fs_xfs_fuzz::block(data, fs_xfs_fuzz::SECTOR),
        fs_xfs_fuzz::superblock(),
        128,
    );
});
