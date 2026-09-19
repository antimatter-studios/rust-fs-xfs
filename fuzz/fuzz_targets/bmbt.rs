#![no_main]
//! A block-map btree block, read with the record count it declares.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    if let Ok(numrecs) = fs_xfs::group_write::leaf_numrecs(&block, 16) {
        let _ = fs_xfs::extent::parse_list(&block, u64::from(numrecs));
    }
});
