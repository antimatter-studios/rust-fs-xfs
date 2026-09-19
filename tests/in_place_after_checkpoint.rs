//! After a mount writes a checkpoint, its in-place writes are refused.
//!
//! A logged operation (create, unlink, rename, truncate to zero, first
//! write into an empty file) writes only its record. The disk doesn't show
//! the change until something replays the log. That's why a mount may
//! write one checkpoint: a second would be built from a disk that doesn't
//! reflect the first.
//!
//! The in-place writes (`write_at`, `set_attributes`, `truncate`) read the
//! same stale disk and weren't fenced by that limit. After
//! `truncate_to_zero`, the inode on disk still has its size and extents, so
//! `write_at` accepted the bytes and reported success. Replay then frees
//! those blocks and empties the file, and the write is gone. An attribute
//! change fares no better: replay puts back the core the record holds,
//! because the in-place edit doesn't move the inode's LSN.
//!
//! `mkfs.xfs -p` makes a file with 64 KiB of data in one extent, in the
//! fs-linux-test-harness guest, which always has it: the fence this covers
//! is only observed on a real one-extent file, so a run without the image
//! is a run that checked nothing rather than a run to pass over.

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::write::AttrChange;
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::oracle;

#[test]
fn in_place_writes_are_refused_once_the_mount_has_logged_a_change() {
    // The protofile, the files it names and the image are all made under
    // `std::env::temp_dir()`, which `scripts/with-test-temp.sh` points at
    // a directory inside this repository — the one tree the guest running
    // mkfs.xfs can see.
    let root = std::env::temp_dir().join(format!("fs_xfs_in_place_fence_{}", std::process::id()));
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

    // Before any checkpoint, the in-place write is allowed: the fence
    // must not refuse what was always safe.
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    assert_eq!(fs.write_at(&inode, &raw, 0, b"before").unwrap(), 6);

    fs.truncate_to_zero(ino).expect("the logged truncate");

    // THE DISK still shows the file as it was: a record is written and
    // nothing is applied in place. Read straight off the device, because
    // the mount itself now reads through what it has logged (#89).
    let mut on_disk = vec![0u8; usize::from(fs.superblock().inodesize)];
    let at = fs.inode_offset(ino).expect("where the inode lives");
    let disk = FileDevice::open(image.to_str().unwrap()).expect("the image itself");
    disk.read_at(at, &mut on_disk).expect("read the disk");
    let disk_size = u64::from_be_bytes(
        on_disk[fs_xfs::inode::offsets::SIZE..fs_xfs::inode::offsets::SIZE + 8]
            .try_into()
            .unwrap(),
    );
    assert_eq!(
        disk_size,
        body.len() as u64,
        "the record was applied in place"
    );

    // THE MOUNT reads the truncate it logged, which is what lets a second
    // journalled operation be built on it (#89).
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    assert_eq!(
        inode.size, 0,
        "the mount should read the size its own record logged"
    );

    let wrote = fs.write_at(&inode, &raw, 0, b"after");
    assert!(
        wrote.is_err(),
        "wrote into blocks the logged truncate has freed: {wrote:?}"
    );
    let attrs = fs.set_attributes(
        &inode,
        &AttrChange {
            permissions: Some(0o600),
            ..Default::default()
        },
    );
    assert!(
        attrs.is_err(),
        "changed an inode core that replay will overwrite: {attrs:?}"
    );
    let shrunk = fs.truncate(&inode, 4096, None);
    assert!(
        shrunk.is_err(),
        "shrank an inode core that replay will overwrite: {shrunk:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}
