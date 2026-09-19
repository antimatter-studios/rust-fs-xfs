#![no_main]
//! A btree root says how many records it holds. The free-space, rmap and
//! refcount trees all read that count and then index by it.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let block = fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK);
    for record_bytes in [8usize, 12, 16, 24] {
        if let Ok(numrecs) = fs_xfs::group_write::leaf_numrecs(&block, record_bytes) {
            let _ = fs_xfs::group_write::leaf_records(&block, numrecs);
            let _ = fs_xfs::rmap::leaf_records(&block, numrecs);
            let _ = fs_xfs::refcount::leaf_records(&block, numrecs);
        }
    }
    // A count the block never agreed to, which is what the `min`
    // backstops inside those three exist to survive.
    for numrecs in [0u16, 1, u16::MAX] {
        let _ = fs_xfs::group_write::leaf_records(&block, numrecs);
        let _ = fs_xfs::rmap::leaf_records(&block, numrecs);
        let _ = fs_xfs::refcount::leaf_records(&block, numrecs);
    }
});
