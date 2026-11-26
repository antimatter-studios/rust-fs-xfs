//! The bounded-memory image helpers (`tests/common/image.rs`, #606): what they
//! report is what the whole-file reads they replaced would have reported.

mod common;

use std::os::unix::fs::{FileExt, MetadataExt};

use common::image::{self, Difference, Image, Snapshot, SMALL_FILE_LIMIT};
use common::scratch::Volume;

const MIB: u64 = 1 << 20;
const SUITE: &str = "image_bytes";

/// A scratch image of `len` bytes, all holes, removed when dropped.
fn blank(name: &str, len: u64) -> Volume {
    Volume::empty(SUITE, &format!("{}-{name}.img", std::process::id()), len)
}

fn write(volume: &Volume, offset: u64, bytes: &[u8]) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(volume.path())
        .unwrap()
        .write_all_at(bytes, offset)
        .unwrap();
}

#[test]
fn a_range_read_returns_the_bytes_at_that_offset() {
    let file = blank("range", 3 * MIB);
    write(&file, 2 * MIB - 2, b"across");
    let image = Image::open(file.path());
    assert_eq!(image.len(), 3 * MIB);
    assert_eq!(image.at(2 * MIB - 2, 6), b"across");
    assert_eq!(image.head().len(), 64 << 10);
}

#[test]
fn the_first_differing_byte_is_named_beyond_the_first_chunk() {
    let left = blank("left", 3 * MIB);
    let right = blank("right", 3 * MIB);
    assert_eq!(image::first_difference(left.path(), right.path()), None);
    write(&right, 2 * MIB + 7, &[9]);
    write(&right, 2 * MIB + 9, &[4]);
    assert_eq!(
        image::first_difference(left.path(), right.path()),
        Some(Difference::Byte {
            offset: 2 * MIB + 7,
            left: 0,
            right: 9
        })
    );
}

#[test]
fn bytes_inside_the_ignored_range_are_not_a_difference() {
    let left = blank("outside-left", 2 * MIB);
    let right = blank("outside-right", 2 * MIB);
    write(&right, MIB - 1, &[1, 2]);
    assert_eq!(
        image::first_difference_outside(left.path(), right.path(), MIB - 1..MIB + 1),
        None
    );
    assert_eq!(
        image::first_difference_outside(left.path(), right.path(), MIB - 1..MIB),
        Some(Difference::Byte {
            offset: MIB,
            left: 0,
            right: 2
        })
    );
}

#[test]
fn files_of_different_lengths_differ() {
    let left = blank("short", 10);
    let right = blank("long", 11);
    assert_eq!(
        image::first_difference(left.path(), right.path()),
        Some(Difference::Length {
            left: 10,
            right: 11
        })
    );
}

#[test]
fn a_snapshot_holds_the_image_as_it_was_and_keeps_its_holes() {
    let file = blank("snapshot", 64 * MIB);
    write(&file, MIB, b"before");
    let before = Snapshot::of(file.path());
    before.assert_unchanged(file.path(), "untouched");
    let copy = std::fs::metadata(before.path()).unwrap();
    assert_eq!(copy.len(), 64 * MIB);
    assert!(
        copy.blocks() * 512 < 16 * MIB,
        "the snapshot of a sparse image took {} bytes on disk",
        copy.blocks() * 512
    );
    write(&file, 40 * MIB, &[1]);
    assert_eq!(
        image::first_difference(before.path(), file.path()),
        Some(Difference::Byte {
            offset: 40 * MIB,
            left: 0,
            right: 1
        })
    );
    let kept = before.path().to_path_buf();
    drop(before);
    assert!(!kept.exists(), "the snapshot is removed when dropped");
}

#[test]
#[should_panic(expected = "the first differing byte is at 5")]
fn an_assertion_over_a_changed_image_names_the_byte() {
    let file = blank("changed", 100);
    let before = Snapshot::of(file.path());
    write(&file, 5, &[1]);
    before.assert_unchanged(file.path(), "changed");
}

#[test]
fn a_file_over_the_limit_is_not_read_whole() {
    let small = blank("small", SMALL_FILE_LIMIT);
    assert_eq!(
        image::small_file(small.path()).unwrap().len() as u64,
        SMALL_FILE_LIMIT
    );
    let large = blank("large", SMALL_FILE_LIMIT + 1);
    assert!(image::small_file(large.path()).is_err());
}

/// The digest of "abc" from FIPS 180-2, appendix B.1.
#[test]
fn the_streamed_digest_is_sha_256() {
    let file = blank("digest", 0);
    write(&file, 0, b"abc");
    assert_eq!(
        image::sha256_file(file.path()),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
}
