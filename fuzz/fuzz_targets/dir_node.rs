#![no_main]
//! Node blocks point at other blocks. A node that points at itself is
//! the cheapest way to turn a directory lookup into a hang, which is
//! what the visit budget added on 2026-09-06 is there to stop.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    let sb = fs_xfs_fuzz::superblock();
    let _ = fs_xfs::dir::parse_node(&block, sb);
    let _ = fs_xfs::dir::verify_da_block(&block, sb, 0, 128);
});
