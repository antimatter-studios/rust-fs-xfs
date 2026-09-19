//! An oracle that unmounts a volume checks that the unmount happened
//! (#206).
//!
//! The superblock's summary counters are lazy: they live in memory while
//! a filesystem is mounted and the kernel writes them at unmount. This
//! driver never writes them. So an unmount that did not happen — a busy
//! mount point, a read-only fallback, a shutdown filesystem — leaves
//! them as they were, and the `xfs_repair -n` that every oracle runs
//! next walks the trees, counts the real free blocks, and disagrees with
//! a superblock the kernel had not finished with.
//!
//! What it prints is `sb_fdblocks N, counted N-1` and nothing else. That
//! reads as a driver fault and was chased as one twice, in #199 and
//! #124.
//!
//! So an unmount is checked, and a mount that failed is reported as a
//! refusal rather than as an absent kernel: `umount "$m" || echo
//! UMOUNT_FAILED`, and the suite asserts on it. This keeps that true for
//! the next oracle as much as for the ones it was written for.

use std::path::{Path, PathBuf};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

#[test]
fn every_guest_script_that_unmounts_says_when_it_could_not() {
    let mut unchecked = Vec::new();
    for entry in std::fs::read_dir(tests_dir()).expect("the tests directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "unmount_checked.rs" {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a test file");
        // The unmounts a script performs, ignoring prose about them.
        let unmounts: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with("//") && l.starts_with("umount "))
            .collect();
        if unmounts.is_empty() {
            continue;
        }
        // One place naming the failure is enough: a script that reports
        // it at all is one whose author faced the question, and the
        // assertion beside it is what this cannot check by reading.
        if !body.contains("UMOUNT_FAILED") {
            unchecked.push(name);
        }
    }
    assert!(
        unchecked.is_empty(),
        "these suites unmount without checking it, so a volume the kernel never \
         finished with is graded as though it had: {unchecked:?}"
    );
}
