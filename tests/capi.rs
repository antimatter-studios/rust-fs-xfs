//! The C ABI, exercised the way a C caller would use it.
//!
//! This layer is what the consuming application actually links against,
//! so a defect here reaches users even though every Rust-level test
//! passes. The sibling EROFS driver shipped with this surface at 0%
//! coverage; these tests exist so this crate does not repeat that.
//!
//! Two classes of behaviour matter here that a safe Rust API never has
//! to think about, and both get more attention below than the happy
//! paths do:
//!
//! - **NULL tolerance.** Every pointer parameter must be checked, not
//!   dereferenced. A caller passing NULL should get a failure, not a
//!   crash inside someone else's process.
//! - **Error reporting.** A C caller has only the return value, the
//!   thread-local message, and the errno. If the errno is wrong the
//!   caller misdiagnoses: reporting EIO for a missing file sends a user
//!   hunting for hardware faults.
//!
//! Fixtures are gitignored, so these skip cleanly on a fresh clone.

use fs_xfs::capi::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::{Path, PathBuf};

/// Errno values the ABI documents. Spelled out rather than imported so
/// the test asserts the contract rather than mirroring the source.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;

fn fixture() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(".vm-share")
        .join("xfsdata-default.img");
    p.exists().then_some(p)
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Mount the fixture, or `None` when it is absent.
fn mount() -> Option<*mut fs_xfs_fs> {
    let path = fixture()?;
    let c = cstr(path.to_str().unwrap());
    let fs = unsafe { fs_xfs_mount(c.as_ptr()) };
    assert!(
        !fs.is_null(),
        "mounting the fixture failed: {}",
        last_error()
    );
    Some(fs)
}

fn last_error() -> String {
    unsafe { CStr::from_ptr(fs_xfs_last_error()) }
        .to_string_lossy()
        .into_owned()
}

fn zeroed_attr() -> fs_xfs_attr_t {
    unsafe { std::mem::zeroed() }
}

// ---------------------------------------------------------------------
// Happy paths
// ---------------------------------------------------------------------

#[test]
fn mounts_and_reports_volume_info() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut info: fs_xfs_volume_info_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_xfs_get_volume_info(fs, &mut info) }, 0);

    assert!(
        info.block_size.is_power_of_two() && info.block_size >= 512,
        "block size {} is not sane",
        info.block_size
    );
    assert!(matches!(info.version, 4 | 5), "version {}", info.version);
    assert!(info.ag_count > 0, "a filesystem has at least one AG");
    assert!(
        info.free_blocks <= info.total_blocks,
        "more free blocks than blocks exist"
    );
    assert!(
        info.free_inodes <= info.inode_count,
        "more free inodes than inodes exist"
    );
    assert_ne!(info.uuid, [0u8; 16], "a real filesystem has a UUID");

    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn stats_a_file_and_a_directory() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };

    let mut a = zeroed_attr();
    assert_eq!(
        unsafe { fs_xfs_stat(fs, cstr("/small.txt").as_ptr(), &mut a) },
        0
    );
    assert_eq!(a.file_type, 1, "small.txt should be a regular file");
    assert!(a.size > 0);
    assert!(a.link_count >= 1);
    assert_ne!(a.inode, 0);

    let mut d = zeroed_attr();
    assert_eq!(unsafe { fs_xfs_stat(fs, cstr("/sub").as_ptr(), &mut d) }, 0);
    assert_eq!(d.file_type, 2, "sub should be a directory");

    // Stat by inode number must agree with stat by path.
    let mut by_ino = zeroed_attr();
    assert_eq!(unsafe { fs_xfs_stat_ino(fs, a.inode, &mut by_ino) }, 0);
    assert_eq!(by_ino.inode, a.inode);
    assert_eq!(by_ino.size, a.size);
    assert_eq!(by_ino.mode, a.mode);

    unsafe { fs_xfs_umount(fs) };
}

/// `stat` must describe the link itself rather than its target,
/// otherwise a caller cannot tell a link from what it points at.
#[test]
fn stat_does_not_follow_symlinks() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut a = zeroed_attr();
    assert_eq!(
        unsafe { fs_xfs_stat(fs, cstr("/link-short").as_ptr(), &mut a) },
        0
    );
    assert_eq!(a.file_type, 7, "link-short should report as a symlink");
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn iterates_a_directory_to_completion() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let iter = unsafe { fs_xfs_dir_open(fs, cstr("/").as_ptr()) };
    assert!(!iter.is_null(), "opening the root failed: {}", last_error());

    let mut names = Vec::new();
    loop {
        let mut e: fs_xfs_dirent_t = unsafe { std::mem::zeroed() };
        let rc = unsafe { fs_xfs_dir_next(iter, &mut e) };
        assert!(rc >= 0, "dir_next failed: {}", last_error());
        if rc == 0 {
            break;
        }
        let name = unsafe { CStr::from_ptr(e.name.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(
            name.len(),
            usize::from(e.name_len),
            "name_len disagrees with the NUL-terminated name"
        );
        assert_ne!(e.inode, 0, "entry `{name}` has inode 0");
        names.push(name);
    }

    // A second call after the end must keep returning 0 rather than
    // wrapping around or erroring.
    let mut e: fs_xfs_dirent_t = unsafe { std::mem::zeroed() };
    assert_eq!(unsafe { fs_xfs_dir_next(iter, &mut e) }, 0);

    unsafe { fs_xfs_dir_close(iter) };

    assert!(names.contains(&"small.txt".to_string()), "got {names:?}");
    assert!(names.contains(&"manyfiles".to_string()), "got {names:?}");
    unsafe { fs_xfs_umount(fs) };
}

/// The 400-entry directory is the one that is not in short form, so it
/// exercises the block/leaf path through the ABI.
#[test]
fn iterates_a_large_directory() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let iter = unsafe { fs_xfs_dir_open(fs, cstr("/manyfiles").as_ptr()) };
    assert!(!iter.is_null(), "{}", last_error());

    let mut count = 0;
    loop {
        let mut e: fs_xfs_dirent_t = unsafe { std::mem::zeroed() };
        if unsafe { fs_xfs_dir_next(iter, &mut e) } != 1 {
            break;
        }
        count += 1;
    }
    unsafe { fs_xfs_dir_close(iter) };
    assert_eq!(count, 400, "expected 400 entries, iterated {count}");
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn reads_file_contents() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/small.txt").as_ptr(),
            0,
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len() as u64,
        )
    };
    assert!(n > 0, "read failed: {}", last_error());
    assert_eq!(&buf[..n as usize], b"hello world\n");

    // Reading from an offset must return the tail, not the head again.
    let mut tail = [0u8; 64];
    let m = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/small.txt").as_ptr(),
            6,
            tail.as_mut_ptr().cast::<c_void>(),
            tail.len() as u64,
        )
    };
    assert_eq!(&tail[..m as usize], b"world\n");

    // At and past end of file, zero bytes and not an error.
    let at_eof = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/small.txt").as_ptr(),
            n as u64,
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len() as u64,
        )
    };
    assert_eq!(at_eof, 0, "a read starting at EOF returns no bytes");

    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn reads_a_symlink_target() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0 as c_char; 512];
    let n = unsafe {
        fs_xfs_readlink(
            fs,
            cstr("/link-short").as_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert!(n > 0, "readlink failed: {}", last_error());
    let target = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    assert_eq!(target, "small.txt");
    assert_eq!(usize::try_from(n).unwrap(), target.len());
    unsafe { fs_xfs_umount(fs) };
}

/// A buffer shorter than the target must truncate and stay
/// NUL-terminated rather than overrun.
#[test]
fn readlink_truncates_into_a_short_buffer() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0x7F as c_char; 5];
    let n = unsafe {
        fs_xfs_readlink(
            fs,
            cstr("/link-short").as_ptr(),
            buf.as_mut_ptr(),
            buf.len(),
        )
    };
    assert_eq!(n, 4, "must fill bufsize - 1 bytes and terminate");
    assert_eq!(buf[4], 0, "the buffer must remain NUL-terminated");
    let s = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy();
    assert_eq!(s, "smal");
    unsafe { fs_xfs_umount(fs) };
}

// ---------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------

/// The errno is the only thing distinguishing "not there" from "this
/// volume is damaged". Getting it wrong sends a user to the wrong place.
#[test]
fn a_missing_path_reports_enoent_not_eio() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut a = zeroed_attr();
    assert_eq!(
        unsafe { fs_xfs_stat(fs, cstr("/no-such-file").as_ptr(), &mut a) },
        -1
    );
    assert_eq!(
        fs_xfs_last_errno(),
        ENOENT,
        "a missing path must be ENOENT, not {}",
        fs_xfs_last_errno()
    );
    assert!(!last_error().is_empty(), "a failure must leave a message");
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn listing_a_file_reports_enotdir() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let iter = unsafe { fs_xfs_dir_open(fs, cstr("/small.txt").as_ptr()) };
    assert!(
        iter.is_null(),
        "a regular file must not open as a directory"
    );
    assert_eq!(fs_xfs_last_errno(), ENOTDIR);
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn reading_a_directory_as_a_file_reports_eisdir() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let mut buf = [0u8; 16];
    let n = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/sub").as_ptr(),
            0,
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len() as u64,
        )
    };
    assert_eq!(n, -1);
    assert_eq!(fs_xfs_last_errno(), EISDIR);
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn mounting_a_non_xfs_file_fails_with_a_message() {
    let tmp = std::env::temp_dir().join(format!("capi-notxfs-{}.img", std::process::id()));
    std::fs::write(&tmp, vec![0x5Au8; 65536]).unwrap();
    let c = cstr(tmp.to_str().unwrap());
    let fs = unsafe { fs_xfs_mount(c.as_ptr()) };
    assert!(fs.is_null(), "a file of 0x5A must not mount as XFS");
    assert_eq!(fs_xfs_last_errno(), EIO);
    assert!(
        last_error().to_lowercase().contains("xfs"),
        "message should name the format: {}",
        last_error()
    );
    std::fs::remove_file(&tmp).ok();
}

#[test]
fn mounting_a_missing_path_fails() {
    let fs = unsafe { fs_xfs_mount(cstr("/nonexistent/device.img").as_ptr()) };
    assert!(fs.is_null());
    assert!(!last_error().is_empty());
}

// ---------------------------------------------------------------------
// NULL tolerance
//
// Every pointer parameter must be checked rather than dereferenced. A
// caller passing NULL should get a failure, not a segfault inside its
// own process.
// ---------------------------------------------------------------------

#[test]
fn null_pointers_fail_instead_of_crashing() {
    let mut attr = zeroed_attr();
    let mut info: fs_xfs_volume_info_t = unsafe { std::mem::zeroed() };
    let mut dirent: fs_xfs_dirent_t = unsafe { std::mem::zeroed() };
    let mut buf = [0u8; 8];
    let mut cbuf = [0 as c_char; 8];
    let p = cstr("/x");

    unsafe {
        assert!(fs_xfs_mount(std::ptr::null()).is_null());
        assert!(fs_xfs_mount_with_callbacks(std::ptr::null()).is_null());

        assert_eq!(fs_xfs_get_volume_info(std::ptr::null_mut(), &mut info), -1);
        assert_eq!(fs_xfs_stat(std::ptr::null_mut(), p.as_ptr(), &mut attr), -1);
        assert_eq!(fs_xfs_stat_ino(std::ptr::null_mut(), 1, &mut attr), -1);
        assert!(fs_xfs_dir_open(std::ptr::null_mut(), p.as_ptr()).is_null());
        assert_eq!(fs_xfs_dir_next(std::ptr::null_mut(), &mut dirent), -1);
        assert_eq!(
            fs_xfs_read_file(
                std::ptr::null_mut(),
                p.as_ptr(),
                0,
                buf.as_mut_ptr().cast::<c_void>(),
                buf.len() as u64
            ),
            -1
        );
        assert_eq!(
            fs_xfs_readlink(
                std::ptr::null_mut(),
                p.as_ptr(),
                cbuf.as_mut_ptr(),
                cbuf.len()
            ),
            -1
        );

        // Releasing NULL must be a safe no-op, as the header promises.
        fs_xfs_umount(std::ptr::null_mut());
        fs_xfs_dir_close(std::ptr::null_mut());
    }
}

#[test]
fn null_output_pointers_fail_instead_of_crashing() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    unsafe {
        assert_eq!(fs_xfs_get_volume_info(fs, std::ptr::null_mut()), -1);
        assert_eq!(
            fs_xfs_stat(fs, cstr("/").as_ptr(), std::ptr::null_mut()),
            -1
        );
        assert_eq!(fs_xfs_stat_ino(fs, 1, std::ptr::null_mut()), -1);
        assert_eq!(
            fs_xfs_read_file(fs, cstr("/small.txt").as_ptr(), 0, std::ptr::null_mut(), 8),
            -1
        );
        assert_eq!(
            fs_xfs_readlink(fs, cstr("/link-short").as_ptr(), std::ptr::null_mut(), 8),
            -1
        );
        // A zero-length buffer leaves no room even for the terminator.
        let mut one = [0 as c_char; 1];
        assert!(fs_xfs_readlink(fs, cstr("/link-short").as_ptr(), one.as_mut_ptr(), 0) < 0);

        // A NULL path is a failure, not a dereference.
        let mut attr = zeroed_attr();
        assert_eq!(fs_xfs_stat(fs, std::ptr::null(), &mut attr), -1);
        assert!(fs_xfs_dir_open(fs, std::ptr::null()).is_null());

        fs_xfs_umount(fs);
    }
}

// ---------------------------------------------------------------------
// The callback mount path
// ---------------------------------------------------------------------

struct FileContext {
    bytes: Vec<u8>,
    /// Set to make every read fail, to prove failures surface.
    fail: bool,
}

unsafe extern "C" fn ctx_read(
    context: *mut c_void,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i32 {
    let ctx = unsafe { &*(context as *const FileContext) };
    if ctx.fail {
        return -1;
    }
    let start = offset as usize;
    let end = start.saturating_add(length as usize);
    if end > ctx.bytes.len() {
        return -1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(
            ctx.bytes[start..end].as_ptr(),
            buf.cast::<u8>(),
            end - start,
        )
    };
    0
}

#[test]
fn mounts_over_a_caller_supplied_reader() {
    let Some(img) = fixture() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let ctx = Box::new(FileContext {
        bytes: std::fs::read(&img).unwrap(),
        fail: false,
    });
    let size = ctx.bytes.len() as u64;
    let cfg = fs_xfs_blockdev_cfg_t {
        read: Some(ctx_read),
        context: Box::into_raw(ctx) as *mut c_void,
        size_bytes: size,
        block_size: 512,
    };
    let fs = unsafe { fs_xfs_mount_with_callbacks(&cfg) };
    assert!(!fs.is_null(), "callback mount failed: {}", last_error());

    // And it must actually work, not merely mount.
    let mut buf = [0u8; 64];
    let n = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/small.txt").as_ptr(),
            0,
            buf.as_mut_ptr().cast::<c_void>(),
            buf.len() as u64,
        )
    };
    assert_eq!(&buf[..n as usize], b"hello world\n");

    unsafe { fs_xfs_umount(fs) };
    drop(unsafe { Box::from_raw(cfg.context as *mut FileContext) });
}

/// A callback that fails must surface as an error, never as silently
/// zeroed data — a caller cannot detect the difference otherwise.
#[test]
fn a_failing_callback_surfaces_as_an_error() {
    let ctx = Box::new(FileContext {
        bytes: vec![0u8; 4096],
        fail: true,
    });
    let cfg = fs_xfs_blockdev_cfg_t {
        read: Some(ctx_read),
        context: Box::into_raw(ctx) as *mut c_void,
        size_bytes: 4096,
        block_size: 512,
    };
    let fs = unsafe { fs_xfs_mount_with_callbacks(&cfg) };
    assert!(fs.is_null(), "a failing reader must not produce a handle");
    assert!(!last_error().is_empty());
    drop(unsafe { Box::from_raw(cfg.context as *mut FileContext) });
}

#[test]
fn a_null_callback_is_rejected() {
    let cfg = fs_xfs_blockdev_cfg_t {
        read: None,
        context: std::ptr::null_mut(),
        size_bytes: 4096,
        block_size: 512,
    };
    assert!(unsafe { fs_xfs_mount_with_callbacks(&cfg) }.is_null());
    assert!(!last_error().is_empty());
}

// ---------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------

/// `fs_xfs_last_error` must never return NULL, including before any
/// failure has occurred — a C caller will print it unconditionally.
#[test]
fn last_error_is_never_null() {
    assert!(!fs_xfs_last_error().is_null());
    let s = last_error();
    assert!(!s.is_empty(), "the initial message must still be printable");
}

/// A non-UTF-8 path is rejected rather than misinterpreted.
#[test]
fn a_non_utf8_path_is_rejected() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    // 0xFF is not valid UTF-8 in any position.
    let bad = [b'/' as c_char, 0xFFu8 as c_char, 0];
    let mut attr = zeroed_attr();
    assert_eq!(unsafe { fs_xfs_stat(fs, bad.as_ptr(), &mut attr) }, -1);
    assert!(!last_error().is_empty());
    unsafe { fs_xfs_umount(fs) };
}
