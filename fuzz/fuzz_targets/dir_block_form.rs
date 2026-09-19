#![no_main]
//! Block-form directories put the leaf entries and the tail in the same
//! block as the data, so one length error reaches two structures.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::dir::parse_block_form(
        &fs_xfs_fuzz::block(data, fs_xfs_fuzz::BLOCK),
        fs_xfs_fuzz::superblock(),
    );
});
