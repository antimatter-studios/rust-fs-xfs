//! Remove a file, for hand inspection.
//!
//! `cargo run --example unlink_file -- <image> <parent-path> <name>`

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [img, parent, name] = <[String; 3]>::try_from(a)
        .unwrap_or_else(|_| panic!("usage: unlink_file <image> <parent-path> <name>"));

    let parent_ino = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).expect("open"))).expect("mount");
        fs.lookup_path(&parent).expect("find the directory").ino
    };

    let dev = FileDevice::open_rw(&img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let (ino, lsn) = fs.unlink_file(parent_ino, name.as_bytes()).expect("unlink");
    println!(
        "removed {name} (ino {ino}), logged at lsn {lsn:#x} (cycle {}, block {})",
        lsn >> 32,
        lsn as u32
    );
}
