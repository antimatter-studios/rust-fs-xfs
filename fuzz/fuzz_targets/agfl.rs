#![no_main]
//! The free list is a circular buffer with an attacker-controlled head,
//! tail and count, which is a shape that historically wraps.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    let _ = fs_xfs::agfl::Agfl::parse(
        &fs_xfs_fuzz::block(data, fs_xfs_fuzz::SECTOR),
        fs_xfs_fuzz::superblock(),
        fs_xfs_fuzz::agf(),
        0,
    );
});
