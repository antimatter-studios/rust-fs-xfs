//! Rename one entry in a short-form directory, for hand inspection.
//!
//! `cargo run --example rename -- <image> <dir-path> <from> <to>`

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [img, dir, from, to] = <[String; 4]>::try_from(a)
        .unwrap_or_else(|_| panic!("usage: rename <image> <dir-path> <from> <to>"));

    let dev = FileDevice::open_rw(&img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let d = fs.lookup_path(&dir).expect("find the directory");
    let lsn = fs
        .rename_in_directory(d.ino, from.as_bytes(), to.as_bytes())
        .expect("rename");
    println!(
        "logged at lsn {lsn:#x} (cycle {}, block {})",
        lsn >> 32,
        lsn as u32
    );
}
