#![no_main]
//! Directory entry names are bounded by where the *next* entry says it
//! starts, and the tail of the block is an array indexed from the end.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    let sb = fs_xfs_fuzz::superblock();
    let _ = fs_xfs::dir::parse_data_block(&block, sb);
    let _ = fs_xfs::dir::verify_data_block(&block, sb, 0, 128);
});
