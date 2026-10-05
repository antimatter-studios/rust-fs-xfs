//! `truncate` and `write_at` judge the inode as it is now, not the copy the
//! caller read (#330).
//!
//! Both take the caller's `&Inode`. The in-place `truncate` keeps a file's
//! blocks past its new end, so a copy read before a shrink still shows the
//! old size. A grow judged against that copy was let through: the file came
//! back to a size it no longer had, over the bytes the shrink had hidden.
//! A write judged against it landed past the real end of file and reported
//! success.
//!
//! `mkfs.xfs -p` makes a file with 64 KiB of data in one extent, in the
//! fs-linux-test-harness guest, as `in_place_after_checkpoint.rs` does.

use fs_core::BlockDevice;
use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::oracle;

#[test]
fn a_stale_copy_of_the_inode_can_neither_regrow_the_file_nor_write_past_its_end() {
    let root = std::env::temp_dir().join(format!("fs_xfs_stale_inode_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let body: Vec<u8> = (0..64 * 1024).map(|i| (i % 251) as u8).collect();
    std::fs::write(root.join("body"), &body).unwrap();
    std::fs::write(
        root.join("proto"),
        format!(
            "/dev/null\n0 0\nd--755 0 0\ndata ---644 0 0 {}\n$\n",
            root.join("body").display()
        ),
    )
    .unwrap();
    let image = root.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(300 * 1024 * 1024))
        .unwrap();
    let out = oracle("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(root.join("proto"))
        .arg(&image)
        .output();
    assert!(out.ok(), "{}{}", out.stdout, out.stderr);

    let dev = Arc::new(FileDevice::open_rw(image.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let ino = fs.lookup_path("/data").expect("the file").ino;

    // The copy a caller holds, read while the file is 64 KiB.
    let (stale, stale_raw) = fs.read_inode_raw(ino).unwrap();
    assert_eq!(stale.size, body.len() as u64);

    fs.truncate(&stale, 4096, None).expect("the in-place shrink");
    assert_eq!(fs.read_inode_raw(ino).unwrap().0.size, 4096);

    let regrown = fs.truncate(&stale, 32 * 1024, None);
    assert!(
        regrown.is_err(),
        "a stale copy regrew the file from 4096 to 32768 over the bytes the shrink hid"
    );
    assert_eq!(
        fs.read_inode_raw(ino).unwrap().0.size,
        4096,
        "the size moved although the grow was refused"
    );

    let wrote = fs.write_at(&stale, &stale_raw, 8192, b"past the end");
    assert!(
        wrote.is_err(),
        "a stale copy wrote at 8192 into a 4096-byte file and reported {wrote:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
