#![no_main]
//! An inode as it appears inside a log item: a native-endian core with
//! big-endian forks behind it. Replay reads this from a dirty log before
//! the filesystem is mountable, which is as early as untrusted bytes get.
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data: &[u8]| {
    // Genuinely variable length: a log item is as long as its header says.
    let _ = fs_xfs::log_write::log_dinode_from_disk(data);
});
