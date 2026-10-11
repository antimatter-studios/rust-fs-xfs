//! A scratch volume is copied with its holes left as holes (#454).
//!
//! A fixture is a 400 MiB image holding a few MiB of metadata, and
//! `scratch::Volume::copy_of` copied it with `std::fs::copy`. On a
//! filesystem that can share extents that is a reflink and costs nothing,
//! which is why nobody saw it; anywhere else it writes every zero. Over the
//! guest's 9p share that is 28 seconds a copy, measured, and the feature
//! matrix alone makes hundreds: the in-guest suite ran into its 150-minute
//! limit.
//!
//! What the copy is asked to write is counted, so the answer does not
//! depend on what the filesystem underneath does with zeros.

mod common;

use common::scratch::{copy_sparsely, WriteAt};

const LEN: u64 = 64 << 20;

/// A destination that keeps what it is given and counts it.
struct Counted {
    image: Vec<u8>,
    written: u64,
}

impl WriteAt for Counted {
    fn write_at(&mut self, bytes: &[u8], at: u64) -> std::io::Result<()> {
        self.image[at as usize..at as usize + bytes.len()].copy_from_slice(bytes);
        self.written += bytes.len() as u64;
        Ok(())
    }
}

#[test]
fn a_copy_writes_the_data_a_sparse_image_holds_and_not_its_holes() {
    let mut source = vec![0u8; LEN as usize];
    // Data at the start, across a piece boundary in the middle, and in the
    // last bytes: the last is what a copy that stops at the last data it
    // saw gets wrong.
    for at in [0, (LEN / 2 - 100) as usize, (LEN - 7) as usize] {
        source[at..at + 7].copy_from_slice(b"written");
    }
    let mut dest = Counted {
        image: vec![0u8; LEN as usize],
        written: 0,
    };
    copy_sparsely(&source[..], LEN, &mut dest).unwrap();
    assert!(dest.image == source, "the copy reads other than the source");
    assert!(
        dest.written <= 4 * (64 << 10),
        "a copy of 21 bytes of data in four 64 KiB pieces wrote {} bytes: the holes were \
         written as zeros",
        dest.written
    );
}
