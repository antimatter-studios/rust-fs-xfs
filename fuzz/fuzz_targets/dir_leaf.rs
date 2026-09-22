#![no_main]
//! A leaf block's entry count and stale count are both in the header and
//! both believed by the walk over the entries behind them.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    let sb = fs_xfs_fuzz::superblock();
    let _ = fs_xfs::dir::parse_leaf(&block, sb);
    let _ = fs_xfs::dir::verify_da_block(&block, sb, 0, 128);
});
