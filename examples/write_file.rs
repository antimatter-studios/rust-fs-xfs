//! Give an empty file contents, for hand inspection.
//!
//! `cargo run --example write_file -- <image> <path> <bytes>`

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [img, path, bytes] = <[String; 3]>::try_from(a)
        .unwrap_or_else(|_| panic!("usage: write_file <image> <path> <bytes>"));
    let bytes: usize = bytes.parse().expect("a byte count");

    let ino = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).expect("open"))).expect("mount");
        fs.lookup_path(&path).expect("find the file").ino
    };

    let data: Vec<u8> = (0..bytes).map(|i| (i % 251 + 1) as u8).collect();
    let dev = FileDevice::open_rw(&img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let lsn = fs.write_into_empty_file(ino, &data).expect("write");
    println!(
        "wrote {bytes} bytes to ino {ino}, logged at lsn {lsn:#x} (cycle {}, block {})",
        lsn >> 32,
        lsn as u32
    );
}
