//! The journalled write operations through the C ABI (#460): a writable
//! mount over an `FsCoreDevice`, the namespace calls, allocating writes,
//! truncate to any length, attribute changes after a logged operation,
//! POSIX ACLs through the attribute calls, a flush and a clean unmount.
//!
//! What is pinned here is the ABI's own contract: the value each call
//! returns and the errno each refusal leaves, which a caller acts on and
//! which nothing but this crate decides. The volume is made by this
//! crate's own `mkfs` on an in-memory device, so these cases need no tool
//! and no fixture. What `xfs_repair` and the kernel make of the same
//! calls is `tests/capi_write_ops_oracle.rs`.

use fs_core::ffi::FsCoreDevice;
use fs_core::{BlockDevice, BlockRead};
use fs_xfs::capi::*;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::ffi::{c_char, CString};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

/// Darwin and Linux disagree on these, and the ABI reports the host's.
const ENOTEMPTY: i32 = if cfg!(target_os = "macos") { 66 } else { 39 };
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };
const ENAMETOOLONG: i32 = if cfg!(target_os = "macos") { 63 } else { 36 };
const ENOATTR: i32 = if cfg!(target_os = "macos") { 93 } else { 61 };
const EPERM: i32 = 1;
const ENOENT: i32 = 2;
const EEXIST: i32 = 17;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const EROFS: i32 = 30;

/// A sparse in-memory device, writable or not.
struct Sparse {
    sectors: Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>,
    writable: bool,
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        assert!(self.writable, "a read-only device was written to");
        let mut sectors = self.sectors.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]))[within..within + n]
                .copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        self.writable
    }
}

/// A freshly made volume.
fn formatted() -> Arc<Sparse> {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
        writable: true,
    });
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    dev
}

/// The same bytes behind a device that refuses writes.
fn read_only_copy(dev: &Sparse) -> Arc<Sparse> {
    Arc::new(Sparse {
        sectors: Mutex::new(dev.sectors.lock().unwrap().clone()),
        writable: false,
    })
}

/// A writable C handle over `dev`, mounted through the new entry point.
fn mount_rw(dev: &Arc<Sparse>) -> *mut fs_xfs_fs {
    let handle = FsCoreDevice::into_handle(dev.clone() as Arc<dyn BlockDevice>);
    let fs = unsafe { fs_xfs_mount_rw_with_fs_core_device(handle) };
    // The mount holds its own reference to the device.
    unsafe { fs_core::ffi::fs_core_device_close(handle) };
    assert!(
        !fs.is_null(),
        "mount_rw_with_fs_core_device failed: errno {}",
        fs_xfs_last_errno()
    );
    fs
}

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

fn stat(fs: *mut fs_xfs_fs, path: &str) -> Option<fs_xfs_attr_t> {
    let mut out = std::mem::MaybeUninit::<fs_xfs_attr_t>::zeroed();
    let p = c(path);
    (unsafe { fs_xfs_stat(fs, p.as_ptr(), out.as_mut_ptr()) } == 0)
        .then(|| unsafe { out.assume_init() })
}

fn read_all(fs: *mut fs_xfs_fs, path: &str) -> Vec<u8> {
    let size = stat(fs, path).expect("stat").size as usize;
    let mut buf = vec![0xEEu8; size];
    let p = c(path);
    let n = unsafe { fs_xfs_read_file(fs, p.as_ptr(), buf.as_mut_ptr().cast(), 0, size as u64) };
    assert_eq!(n, size as i64, "read {path}");
    buf
}

fn write(fs: *mut fs_xfs_fs, path: &str, offset: u64, data: &[u8]) -> i64 {
    let p = c(path);
    unsafe {
        fs_xfs_write_file(
            fs,
            p.as_ptr(),
            data.as_ptr().cast(),
            offset,
            data.len() as u64,
        )
    }
}

fn create(fs: *mut fs_xfs_fs, path: &str, mode: u16) -> u64 {
    let p = c(path);
    unsafe { fs_xfs_create(fs, p.as_ptr(), mode) }
}

fn mkdir(fs: *mut fs_xfs_fs, path: &str, mode: u16) -> u64 {
    let p = c(path);
    unsafe { fs_xfs_mkdir(fs, p.as_ptr(), mode) }
}

fn unlink(fs: *mut fs_xfs_fs, path: &str) -> i32 {
    let p = c(path);
    unsafe { fs_xfs_unlink(fs, p.as_ptr()) }
}

fn rmdir(fs: *mut fs_xfs_fs, path: &str) -> i32 {
    let p = c(path);
    unsafe { fs_xfs_rmdir(fs, p.as_ptr()) }
}

fn rename2(fs: *mut fs_xfs_fs, from: &str, to: &str, flags: i32) -> i32 {
    let (a, b) = (c(from), c(to));
    unsafe { fs_xfs_rename2(fs, a.as_ptr(), b.as_ptr(), flags) }
}

fn rename(fs: *mut fs_xfs_fs, from: &str, to: &str) -> i32 {
    let (a, b) = (c(from), c(to));
    unsafe { fs_xfs_rename(fs, a.as_ptr(), b.as_ptr()) }
}

fn link(fs: *mut fs_xfs_fs, from: &str, to: &str) -> i32 {
    let (a, b) = (c(from), c(to));
    unsafe { fs_xfs_link(fs, a.as_ptr(), b.as_ptr()) }
}

fn truncate(fs: *mut fs_xfs_fs, path: &str, size: u64) -> i32 {
    let p = c(path);
    unsafe { fs_xfs_truncate(fs, p.as_ptr(), size, FS_XFS_LEAVE_TIME, 0) }
}

#[track_caller]
fn assert_errno(result: i64, errno: i32, what: &str) {
    assert!(
        result == -1 || result == 0,
        "{what}: expected a failure, got {result}"
    );
    assert_eq!(fs_xfs_last_errno(), errno, "{what}");
}

/// The VFS form of an access ACL: owner rw-, a named user r--, owning
/// group r--, mask r--, other none.
fn vfs_acl(entries: &[(u16, u16, u32)]) -> Vec<u8> {
    let mut out = 2u32.to_le_bytes().to_vec();
    for &(tag, perm, id) in entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&perm.to_le_bytes());
        out.extend_from_slice(&id.to_le_bytes());
    }
    out
}

const UNDEFINED: u32 = u32::MAX;

#[test]
fn the_writable_device_mount_refuses_a_null_handle_and_a_read_only_device() {
    let fs = unsafe { fs_xfs_mount_rw_with_fs_core_device(std::ptr::null_mut()) };
    assert!(fs.is_null());
    assert_ne!(fs_xfs_last_errno(), 0);

    let dev = formatted();
    let ro = read_only_copy(&dev);
    let handle = FsCoreDevice::into_handle(ro as Arc<dyn BlockDevice>);
    let fs = unsafe { fs_xfs_mount_rw_with_fs_core_device(handle) };
    unsafe { fs_core::ffi::fs_core_device_close(handle) };
    assert!(fs.is_null(), "a read-only device mounted for writing");
    assert_eq!(fs_xfs_last_errno(), EROFS);

    let fs = mount_rw(&dev);
    assert_eq!(unsafe { fs_xfs_is_writable(fs) }, 1);
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn create_and_mkdir_answer_the_new_inode_and_refuse_by_errno() {
    let dev = formatted();
    let fs = mount_rw(&dev);

    let dir = mkdir(fs, "/d", 0o750);
    assert_ne!(dir, 0, "mkdir: errno {}", fs_xfs_last_errno());
    let file = create(fs, "/d/f", 0o640);
    assert_ne!(file, 0, "create: errno {}", fs_xfs_last_errno());

    let d = stat(fs, "/d").expect("stat /d");
    assert_eq!(d.inode, dir);
    assert_eq!(d.mode, 0o040750);
    assert_eq!(d.link_count, 2);
    let f = stat(fs, "/d/f").expect("stat /d/f");
    assert_eq!(f.inode, file);
    assert_eq!(f.mode, 0o100640);
    assert_eq!(f.size, 0);

    assert_errno(
        create(fs, "/d/f", 0o644) as i64,
        EEXIST,
        "create over a name",
    );
    assert_errno(mkdir(fs, "/d", 0o755) as i64, EEXIST, "mkdir over a name");
    assert_errno(create(fs, "/", 0o644) as i64, EEXIST, "create the root");
    assert_errno(
        create(fs, "/nope/f", 0o644) as i64,
        ENOENT,
        "missing parent",
    );
    assert_errno(
        create(fs, "/d/f/g", 0o644) as i64,
        ENOTDIR,
        "parent is a file",
    );
    let long = format!("/{}", "n".repeat(256));
    assert_errno(
        create(fs, &long, 0o644) as i64,
        ENAMETOOLONG,
        "256-byte name",
    );
    assert_errno(
        create(fs, "/x", 0o040644) as i64,
        ENOTSUP,
        "a directory's type bits in a file create",
    );
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn a_write_fills_holes_and_grows_the_file_and_truncate_goes_both_ways() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(create(fs, "/f", 0o644), 0);

    assert_eq!(write(fs, "/f", 0, b"hello"), 5, "write into an empty file");
    assert_eq!(write(fs, "/f", 10000, b"tail"), 4, "write past the end");
    assert_eq!(
        write(fs, "/f", 1, b"ELL"),
        3,
        "overwrite after a logged change"
    );
    let bytes = read_all(fs, "/f");
    assert_eq!(bytes.len(), 10004);
    assert_eq!(&bytes[..5], b"hELLo");
    assert!(
        bytes[5..10000].iter().all(|&b| b == 0),
        "the hole reads as zeros"
    );
    assert_eq!(&bytes[10000..], b"tail");
    assert_eq!(write(fs, "/f", 0, b""), 0, "an empty write writes nothing");

    assert_eq!(
        truncate(fs, "/f", 3),
        0,
        "shrink: errno {}",
        fs_xfs_last_errno()
    );
    assert_eq!(read_all(fs, "/f"), b"hEL");
    assert_eq!(
        truncate(fs, "/f", 9000),
        0,
        "grow: errno {}",
        fs_xfs_last_errno()
    );
    let grown = read_all(fs, "/f");
    assert_eq!(grown.len(), 9000);
    assert_eq!(&grown[..3], b"hEL");
    assert!(
        grown[3..].iter().all(|&b| b == 0),
        "what a grow adds reads as zeros"
    );

    assert_ne!(mkdir(fs, "/d", 0o755), 0);
    assert_errno(write(fs, "/d", 0, b"x"), EISDIR, "write to a directory");
    assert_errno(truncate(fs, "/d", 0) as i64, EISDIR, "truncate a directory");
    assert_errno(write(fs, "/missing", 0, b"x"), ENOENT, "write to nothing");
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn attributes_change_after_a_logged_operation() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    // A create is logged, and an in-place write is refused after one.
    assert_ne!(create(fs, "/f", 0o644), 0);
    let p = c("/f");
    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            p.as_ptr(),
            0o600,
            1001,
            1002,
            1_600_000_000,
            5,
            1_700_000_000,
            7,
        )
    };
    assert_eq!(rc, 0, "set_attributes: errno {}", fs_xfs_last_errno());
    let f = stat(fs, "/f").expect("stat");
    assert_eq!(f.mode, 0o100600);
    assert_eq!((f.uid, f.gid), (1001, 1002));
    assert_eq!((f.atime, f.mtime), (1_600_000_000, 1_700_000_000));

    // A field left alone stays as it is.
    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            p.as_ptr(),
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            2000,
            FS_XFS_LEAVE_TIME,
            0,
            FS_XFS_LEAVE_TIME,
            0,
        )
    };
    assert_eq!(rc, 0, "set gid: errno {}", fs_xfs_last_errno());
    let f = stat(fs, "/f").expect("stat");
    assert_eq!((f.mode, f.uid, f.gid), (0o100600, 1001, 2000));
    assert_eq!((f.atime, f.mtime), (1_600_000_000, 1_700_000_000));

    // A truncate with a time stamps it.
    let rc = unsafe { fs_xfs_truncate(fs, p.as_ptr(), 0, 1_650_000_000, 0) };
    assert_eq!(rc, 0, "truncate: errno {}", fs_xfs_last_errno());
    assert_eq!(stat(fs, "/f").expect("stat").mtime, 1_650_000_000);
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn unlink_and_rmdir_remove_names_and_refuse_by_errno() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(mkdir(fs, "/d", 0o755), 0);
    assert_ne!(create(fs, "/d/f", 0o644), 0);

    assert_errno(rmdir(fs, "/d") as i64, ENOTEMPTY, "rmdir a full directory");
    assert_errno(rmdir(fs, "/d/f") as i64, ENOTDIR, "rmdir a file");
    assert_errno(unlink(fs, "/d") as i64, EISDIR, "unlink a directory");
    assert_errno(unlink(fs, "/d/missing") as i64, ENOENT, "unlink nothing");
    assert_errno(rmdir(fs, "/") as i64, EINVAL, "rmdir the root");

    assert_eq!(
        unlink(fs, "/d/f"),
        0,
        "unlink: errno {}",
        fs_xfs_last_errno()
    );
    assert!(stat(fs, "/d/f").is_none());
    assert_eq!(fs_xfs_last_errno(), ENOENT);
    assert_eq!(rmdir(fs, "/d"), 0, "rmdir: errno {}", fs_xfs_last_errno());
    assert!(stat(fs, "/d").is_none());
    assert_eq!(stat(fs, "/").expect("root").link_count, 2);
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn rename_moves_replaces_only_when_asked_and_refuses_a_move_beneath_itself() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(mkdir(fs, "/a", 0o755), 0);
    assert_ne!(mkdir(fs, "/b", 0o755), 0);
    assert_ne!(create(fs, "/a/one", 0o644), 0);
    assert_eq!(write(fs, "/a/one", 0, b"first"), 5);
    assert_ne!(create(fs, "/b/two", 0o644), 0);
    assert_eq!(write(fs, "/b/two", 0, b"second"), 6);

    assert_eq!(rename(fs, "/a/one", "/a/uno"), 0, "same directory");
    assert_eq!(read_all(fs, "/a/uno"), b"first");
    assert_eq!(rename(fs, "/a/uno", "/b/one"), 0, "across directories");
    assert!(stat(fs, "/a/uno").is_none());
    assert_errno(
        rename(fs, "/b/one", "/b/two") as i64,
        EEXIST,
        "plain rename over a name",
    );
    assert_eq!(
        read_all(fs, "/b/two"),
        b"second",
        "a refused rename changed nothing"
    );

    assert_eq!(
        rename2(fs, "/b/one", "/b/two", FS_XFS_RENAME_REPLACE),
        0,
        "replace: errno {}",
        fs_xfs_last_errno()
    );
    assert_eq!(read_all(fs, "/b/two"), b"first");
    assert!(stat(fs, "/b/one").is_none());

    assert_eq!(rename(fs, "/b", "/a/b"), 0, "a directory into a sibling");
    assert_eq!(stat(fs, "/a").expect("a").link_count, 3);
    assert_errno(rename(fs, "/a", "/a/b/a") as i64, EINVAL, "beneath itself");
    assert_errno(
        rename2(fs, "/a/b/two", "/a/x", 0x80) as i64,
        EINVAL,
        "unknown flag",
    );
    assert_errno(
        rename(fs, "/a/missing", "/a/y") as i64,
        ENOENT,
        "missing source",
    );
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn link_gives_a_file_a_second_name_and_refuses_a_directory() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(create(fs, "/f", 0o644), 0);
    assert_eq!(write(fs, "/f", 0, b"shared"), 6);
    assert_ne!(mkdir(fs, "/d", 0o755), 0);

    assert_eq!(
        link(fs, "/f", "/d/g"),
        0,
        "link: errno {}",
        fs_xfs_last_errno()
    );
    assert_eq!(stat(fs, "/f").expect("f").link_count, 2);
    assert_eq!(read_all(fs, "/d/g"), b"shared");
    assert_errno(link(fs, "/f", "/d/g") as i64, EEXIST, "link over a name");
    assert_errno(link(fs, "/d", "/e") as i64, EPERM, "link a directory");
    assert_errno(link(fs, "/missing", "/e") as i64, ENOENT, "link nothing");

    assert_eq!(unlink(fs, "/f"), 0);
    assert_eq!(stat(fs, "/d/g").expect("g").link_count, 1);
    assert_eq!(read_all(fs, "/d/g"), b"shared");
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn a_posix_acl_is_set_read_and_removed_through_the_attribute_calls() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(create(fs, "/f", 0o644), 0);
    let (p, name) = (c("/f"), c("system.posix_acl_access"));
    let acl = vfs_acl(&[
        (0x01, 6, UNDEFINED),
        (0x02, 4, 1000),
        (0x04, 4, UNDEFINED),
        (0x10, 4, UNDEFINED),
        (0x20, 0, UNDEFINED),
    ]);
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
    assert_eq!(rc, 0, "set the ACL: errno {}", fs_xfs_last_errno());
    assert_eq!(
        stat(fs, "/f").expect("stat").mode,
        0o100640,
        "the mode follows the ACL"
    );

    let mut back = vec![0u8; 64];
    let n = unsafe {
        fs_xfs_getxattr(
            fs,
            p.as_ptr(),
            name.as_ptr(),
            back.as_mut_ptr().cast(),
            back.len(),
        )
    };
    assert_eq!(
        n,
        acl.len() as i64,
        "get the ACL: errno {}",
        fs_xfs_last_errno()
    );
    assert_eq!(&back[..n as usize], &acl[..]);

    let rc = unsafe {
        fs_xfs_setxattr(
            fs,
            p.as_ptr(),
            name.as_ptr(),
            acl.as_ptr().cast(),
            acl.len(),
            1,
        )
    };
    assert_errno(rc as i64, EEXIST, "create-only over an ACL");
    let garbage = [1u8, 2, 3];
    let rc =
        unsafe { fs_xfs_setxattr(fs, p.as_ptr(), name.as_ptr(), garbage.as_ptr().cast(), 3, 0) };
    assert_errno(rc as i64, EINVAL, "an ACL that is not the VFS format");

    assert_eq!(
        unsafe { fs_xfs_removexattr(fs, p.as_ptr(), name.as_ptr()) },
        0
    );
    let n = unsafe { fs_xfs_getxattr(fs, p.as_ptr(), name.as_ptr(), std::ptr::null_mut(), 0) };
    assert_errno(n, ENOATTR, "the ACL is gone");
    let rc = unsafe { fs_xfs_removexattr(fs, p.as_ptr(), name.as_ptr()) };
    assert_errno(rc as i64, ENOATTR, "remove an ACL that is not there");
    unsafe { fs_xfs_umount(fs) };
}

/// The handle a read-only mount gives refuses every write with EROFS
/// and changes nothing.
#[test]
fn a_read_only_handle_refuses_every_write_with_erofs() {
    let dev = formatted();
    {
        let fs = mount_rw(&dev);
        assert_ne!(create(fs, "/f", 0o644), 0);
        assert_ne!(mkdir(fs, "/d", 0o755), 0);
        unsafe { fs_xfs_umount(fs) };
    }
    let handle = FsCoreDevice::into_handle(read_only_copy(&dev) as Arc<dyn BlockDevice>);
    let fs = unsafe { fs_xfs_mount_with_fs_core_device(handle) };
    unsafe { fs_core::ffi::fs_core_device_close(handle) };
    assert!(
        !fs.is_null(),
        "read-only mount: errno {}",
        fs_xfs_last_errno()
    );
    assert_eq!(unsafe { fs_xfs_is_writable(fs) }, 0);

    assert_errno(create(fs, "/g", 0o644) as i64, EROFS, "create");
    assert_errno(mkdir(fs, "/e", 0o755) as i64, EROFS, "mkdir");
    assert_errno(unlink(fs, "/f") as i64, EROFS, "unlink");
    assert_errno(rmdir(fs, "/d") as i64, EROFS, "rmdir");
    assert_errno(rename(fs, "/f", "/h") as i64, EROFS, "rename");
    assert_errno(link(fs, "/f", "/h") as i64, EROFS, "link");
    assert_errno(write(fs, "/f", 0, b"x"), EROFS, "write");
    assert_errno(truncate(fs, "/f", 1) as i64, EROFS, "truncate");
    assert_errno(unsafe { fs_xfs_flush(fs) } as i64, EROFS, "flush");
    assert!(stat(fs, "/f").is_some() && stat(fs, "/h").is_none());
    unsafe { fs_xfs_umount(fs) };
}

/// THE UNMOUNT IS CLEAN. A writable mount refuses a volume whose log
/// still needs replay, so a handle that logged changes and was released
/// without an unmount record leaves a volume no writable mount takes.
#[test]
fn a_released_writable_handle_leaves_a_volume_a_writable_mount_takes() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    assert_ne!(mkdir(fs, "/d", 0o755), 0);
    assert_ne!(create(fs, "/d/f", 0o644), 0);
    assert_eq!(write(fs, "/d/f", 0, &[7u8; 20000]), 20000);
    assert_eq!(
        unsafe { fs_xfs_flush(fs) },
        0,
        "flush: errno {}",
        fs_xfs_last_errno()
    );
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(fs_xfs_last_errno(), 0, "the unmount failed");

    let again = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a writable mount refused the volume a released handle left");
    let f = again.lookup_path("/d/f").expect("the file survived");
    assert_eq!(f.size, 20000);
    assert!(!again.was_replayed());
}

fn volume_info(fs: *mut fs_xfs_fs) -> fs_xfs_volume_info_t {
    let mut out = std::mem::MaybeUninit::<fs_xfs_volume_info_t>::zeroed();
    assert_eq!(unsafe { fs_xfs_get_volume_info(fs, out.as_mut_ptr()) }, 0);
    unsafe { out.assume_init() }
}

/// The free counts follow what the mount writes. The superblock's are
/// lazy, written at unmount, so a writable handle reporting them would
/// show the volume as it was mounted however much was written since.
#[test]
fn free_space_follows_what_a_mount_writes() {
    let dev = formatted();
    let fs = mount_rw(&dev);
    let before = volume_info(fs);
    assert!(before.free_blocks > 0 && before.free_blocks < before.total_blocks);
    assert_ne!(create(fs, "/big", 0o644), 0);
    let mib = vec![0x5Au8; 1 << 20];
    assert_eq!(write(fs, "/big", 0, &mib), 1 << 20);
    let after = volume_info(fs);
    let used = before.free_blocks - after.free_blocks;
    assert!(
        used >= (1 << 20) / u64::from(before.block_size),
        "1 MiB written and {used} blocks taken"
    );
    assert_eq!(
        after.inode_count - after.free_inodes,
        before.inode_count - before.free_inodes + 1,
        "one inode more in use"
    );
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn the_new_entry_points_reject_null_handles() {
    let p = c("/x");
    let null: *const c_char = std::ptr::null();
    let fs = mount_rw(&formatted());
    let refused: [(&str, i64); 11] = unsafe {
        [
            (
                "create",
                fs_xfs_create(std::ptr::null_mut(), p.as_ptr(), 0o644) as i64,
            ),
            (
                "mkdir",
                fs_xfs_mkdir(std::ptr::null_mut(), p.as_ptr(), 0o755) as i64,
            ),
            (
                "unlink",
                fs_xfs_unlink(std::ptr::null_mut(), p.as_ptr()).into(),
            ),
            (
                "rmdir",
                fs_xfs_rmdir(std::ptr::null_mut(), p.as_ptr()).into(),
            ),
            (
                "rename",
                fs_xfs_rename(std::ptr::null_mut(), p.as_ptr(), p.as_ptr()).into(),
            ),
            (
                "rename2",
                fs_xfs_rename2(std::ptr::null_mut(), p.as_ptr(), p.as_ptr(), 0).into(),
            ),
            (
                "link",
                fs_xfs_link(std::ptr::null_mut(), p.as_ptr(), p.as_ptr()).into(),
            ),
            ("flush", fs_xfs_flush(std::ptr::null_mut()).into()),
            ("create path", fs_xfs_create(fs, null, 0o644) as i64),
            ("rename path", fs_xfs_rename(fs, null, p.as_ptr()).into()),
            ("link path", fs_xfs_link(fs, p.as_ptr(), null).into()),
        ]
    };
    unsafe { fs_xfs_umount(fs) };
    for (what, result) in refused {
        assert!(
            result == 0 || result == -1,
            "{what} accepted a NULL: {result}"
        );
    }
}
