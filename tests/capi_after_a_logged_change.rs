//! A writable C handle keeps working after it has logged a change, and
//! leaves a clean log when it is released (#460).
//!
//! `fs_xfs_setxattr` is a logged operation, so after one the mount refuses
//! every in-place write: the disk it would read is out of date (#186).
//! The C ABI's attribute, truncate and write calls must reach the logged
//! paths the library has for them, or a caller that sets one extended
//! attribute can change nothing else until it mounts the volume again.
//! And `fs_xfs_umount` must write the unmount record, or the volume it
//! leaves has a log a writable mount refuses.
//!
//! Every case uses an entry point the header already declares; the
//! volume is made by this crate's own `mkfs` in a temporary file.

use fs_core::{BlockDevice, FileDevice};
use fs_xfs::capi::*;
use fs_xfs::Filesystem;
use std::ffi::CString;
use std::path::PathBuf;
use std::sync::Arc;

const BYTES: u64 = 300 * 1024 * 1024;

/// A volume in a temporary file, removed when dropped.
struct Image(PathBuf);

impl Image {
    fn new(tag: &str) -> Image {
        let path = std::env::temp_dir().join(format!(
            "fs-xfs-capi-logged-{}-{tag}.img",
            std::process::id()
        ));
        std::fs::File::create(&path)
            .and_then(|f| f.set_len(BYTES))
            .expect("make the image file");
        let dev = FileDevice::open_rw(path.to_str().unwrap()).expect("open");
        fs_xfs::mkfs::format(&dev, &fs_xfs::mkfs::Options::default()).expect("mkfs");
        // One file to change, written in place before anything is logged.
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        let (ino, _) = fs.create_file(root, b"f", 0o644).expect("create");
        fs.write(ino, 0, b"hello").expect("write");
        fs.unmount().expect("unmount");
        Image(path)
    }

    fn mount(&self) -> *mut fs_xfs_fs {
        let p = CString::new(self.0.to_str().unwrap()).unwrap();
        let fs = unsafe { fs_xfs_mount_rw(p.as_ptr()) };
        assert!(!fs.is_null(), "mount_rw: errno {}", fs_xfs_last_errno());
        fs
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Log one change through the C ABI: an extended attribute.
fn log_something(fs: *mut fs_xfs_fs) {
    let (p, name) = (CString::new("/f").unwrap(), CString::new("user.k").unwrap());
    let rc = unsafe { fs_xfs_setxattr(fs, p.as_ptr(), name.as_ptr(), b"v".as_ptr().cast(), 1, 0) };
    assert_eq!(rc, 0, "setxattr: errno {}", fs_xfs_last_errno());
}

fn path() -> CString {
    CString::new("/f").unwrap()
}

#[test]
fn attributes_change_after_a_logged_change() {
    let image = Image::new("attrs");
    let fs = image.mount();
    log_something(fs);
    let p = path();
    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            p.as_ptr(),
            0o600,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE_TIME,
            0,
            1_700_000_000,
            0,
        )
    };
    let errno = fs_xfs_last_errno();
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(rc, 0, "set_attributes after a logged change: errno {errno}");
}

#[test]
fn a_write_grows_the_file_after_a_logged_change() {
    let image = Image::new("grow");
    let fs = image.mount();
    log_something(fs);
    let p = path();
    let n = unsafe { fs_xfs_write_file(fs, p.as_ptr(), b" world".as_ptr().cast(), 5, 6) };
    let errno = fs_xfs_last_errno();
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(n, 6, "a write past the end: errno {errno}");
}

#[test]
fn truncate_grows_and_shrinks_after_a_logged_change() {
    let image = Image::new("truncate");
    let fs = image.mount();
    log_something(fs);
    let p = path();
    let up = unsafe { fs_xfs_truncate(fs, p.as_ptr(), 9000, FS_XFS_LEAVE_TIME, 0) };
    let up_errno = fs_xfs_last_errno();
    let down = unsafe { fs_xfs_truncate(fs, p.as_ptr(), 2, FS_XFS_LEAVE_TIME, 0) };
    let down_errno = fs_xfs_last_errno();
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(up, 0, "truncate up: errno {up_errno}");
    assert_eq!(down, 0, "truncate down: errno {down_errno}");
}

#[test]
fn a_posix_acl_is_set_through_the_attribute_call() {
    let image = Image::new("acl");
    let fs = image.mount();
    let mut acl = 2u32.to_le_bytes().to_vec();
    for (tag, perm, id) in [
        (0x01u16, 6u16, u32::MAX),
        (0x02, 4, 1000),
        (0x04, 4, u32::MAX),
        (0x10, 4, u32::MAX),
        (0x20, 0, u32::MAX),
    ] {
        acl.extend_from_slice(&tag.to_le_bytes());
        acl.extend_from_slice(&perm.to_le_bytes());
        acl.extend_from_slice(&id.to_le_bytes());
    }
    let (p, name) = (path(), CString::new("system.posix_acl_access").unwrap());
    let rc = unsafe {
        fs_xfs_setxattr(
            fs,
            p.as_ptr(),
            name.as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            0,
        )
    };
    let errno = fs_xfs_last_errno();
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(rc, 0, "setxattr of an access ACL: errno {errno}");
}

#[test]
fn a_released_handle_leaves_a_log_a_writable_mount_takes() {
    let image = Image::new("umount");
    let fs = image.mount();
    log_something(fs);
    unsafe { fs_xfs_umount(fs) };
    let dev = FileDevice::open_rw(image.0.to_str().unwrap()).expect("open");
    let again = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>);
    assert!(
        again.is_ok(),
        "the volume a released handle left is refused for writing: {:?}",
        again.err()
    );
}
