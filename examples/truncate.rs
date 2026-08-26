//! Truncate a file to nothing, for hand inspection.
//!
//! `cargo run --example truncate -- <image> <path>`

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [img, path] =
        <[String; 2]>::try_from(a).unwrap_or_else(|_| panic!("usage: truncate <image> <path>"));

    let ino = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).expect("open"))).expect("mount");
        let found = fs.lookup_path(&path).expect("find the file");
        println!(
            "{path}: ino {} size {} nblocks {}",
            found.ino, found.size, found.nblocks
        );
        found.ino
    };

    let dev = FileDevice::open_rw(&img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let lsn = fs.truncate_to_zero(ino).expect("truncate");
    println!(
        "logged at lsn {lsn:#x} (cycle {}, block {})",
        lsn >> 32,
        lsn as u32
    );
}
