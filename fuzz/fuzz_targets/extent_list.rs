#![no_main]
//! `count` comes from the inode's extent count and the buffer from its
//! fork, and nothing guarantees the two agree. The large counts are
//! there for the multiplication that sizes the read.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    for count in [0u64, 1, 16, 4096, u64::MAX / 16, u64::MAX] {
        let _ = fs_xfs::extent::parse_list(data, count);
    }
});
