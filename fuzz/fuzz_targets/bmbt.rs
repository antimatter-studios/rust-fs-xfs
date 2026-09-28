#![no_main]
//! A block-map btree leaf, read with the record count it declares.
//!
//! The records are taken from behind the block's header, the way the
//! tree walk takes them. This target used to hand `extent::parse_list`
//! the whole block, so the header was decoded as records and the
//! fuzzer's mutations of the real records were never reached (#258).
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    let _ = fs_xfs::bmbt::leaf_records_unverified(&block, fs_xfs_fuzz::superblock().is_v5());
});
