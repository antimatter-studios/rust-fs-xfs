//! Write one logged inode-core change into an image, for hand
//! inspection in the oracle VM.
//!
//! `cargo run --example log_append -- <image> [mode]`
//!
//! The test suite's own copy of this is deliberately transient — it
//! removes its working image so unrelated suites do not trip over it.
//! This keeps one around to look at.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

const DI_MODE: usize = 2;
const PERM_BITS: u16 = 0o7777;

fn main() {
    let mut args = std::env::args().skip(1);
    let img = args.next().expect("usage: log_append <image> [mode]");
    let want = args
        .next()
        .map(|m| u16::from_str_radix(&m, 8).expect("octal mode"))
        .unwrap_or(0o751);

    let dev = FileDevice::open_rw(&img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let root = fs.superblock().rootino;

    let head = fs_xfs::log::head(fs.device(), fs.superblock()).expect("find the log head");
    println!("head: {head:?}");

    let (_, mut raw) = fs.read_inode_raw(root).expect("read the root inode");
    let before = u16::from_be_bytes(raw[DI_MODE..DI_MODE + 2].try_into().unwrap());
    let mode = (before & !PERM_BITS) | want;
    raw[DI_MODE..DI_MODE + 2].copy_from_slice(&mode.to_be_bytes());

    let lsn = fs.log_inode_core(root, &raw).expect("log the core");
    println!(
        "inode {root} at {:#x}: mode {:o} -> {:o}, logged at lsn {:#x} (cycle {}, block {})",
        fs.inode_offset(root).unwrap(),
        before & PERM_BITS,
        want,
        lsn,
        lsn >> 32,
        lsn as u32
    );
}
