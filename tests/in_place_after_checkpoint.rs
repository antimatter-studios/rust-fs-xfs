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
//! `mkfs.xfs -p` makes a file with 64 KiB of data in one extent. Skips when
//! xfsprogs is not installed.

use fs_core::{BlockDevice, FileDevice};
use fs_xfs::write::AttrChange;
use fs_xfs::Filesystem;
use std::process::Command;
use std::sync::Arc;

#[test]
fn in_place_writes_are_refused_once_the_mount_has_logged_a_change() {
    if !Command::new("mkfs.xfs")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        eprintln!("skip: xfsprogs not installed");
        return;
    }
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
    let out = Command::new("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(root.join("proto"))
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let dev = Arc::new(FileDevice::open_rw(image.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let ino = fs.lookup_path("/data").expect("the file").ino;

    // Before any checkpoint, the in-place write is allowed: the fence
    // must not refuse what was always safe.
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    assert_eq!(fs.write_at(&inode, &raw, 0, b"before").unwrap(), 6);

    fs.truncate_to_zero(ino).expect("the logged truncate");

    // The disk still shows the file as it was.
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    assert_eq!(
        inode.size,
        body.len() as u64,
        "the record was applied in place"
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
