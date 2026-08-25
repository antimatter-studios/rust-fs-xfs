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
const ERANGE: i32 = 34;

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

fn last_errno_erange() -> i32 {
    fs_xfs_last_errno()
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
        let ptr = unsafe { fs_xfs_dir_next(iter) };
        if ptr.is_null() {
            assert_eq!(
                fs_xfs_last_errno(),
                0,
                "a clean end of directory must not set an errno: {}",
                last_error()
            );
            break;
        }
        let e = unsafe { &*ptr };
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
    assert!(unsafe { fs_xfs_dir_next(iter) }.is_null());

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
        if unsafe { fs_xfs_dir_next(iter) }.is_null() {
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
            buf.as_mut_ptr().cast::<c_void>(),
            0,
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
            tail.as_mut_ptr().cast::<c_void>(),
            6,
            tail.len() as u64,
        )
    };
    assert_eq!(&tail[..m as usize], b"world\n");

    // At and past end of file, zero bytes and not an error.
    let at_eof = unsafe {
        fs_xfs_read_file(
            fs,
            cstr("/small.txt").as_ptr(),
            buf.as_mut_ptr().cast::<c_void>(),
            n as u64,
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

/// A buffer too small for the target is REFUSED rather than truncated.
///
/// A truncated symlink target is a path to somewhere else, and a caller
/// following it has no way to tell. ERANGE tells it to retry with a
/// larger buffer, which is the standard idiom — and matches the sibling
/// EROFS driver, so the family agrees.
#[test]
fn readlink_refuses_a_buffer_too_small_for_the_target() {
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
    assert_eq!(n, -1, "a target that does not fit must be refused");
    assert_eq!(
        last_errno_erange(),
        ERANGE,
        "a buffer too small is ERANGE, got {}",
        last_errno_erange()
    );
    assert!(
        buf.iter().all(|&c| c as u8 == 0x7F),
        "a refused readlink must not have written into the buffer"
    );
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
            buf.as_mut_ptr().cast::<c_void>(),
            0,
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
        assert!(fs_xfs_dir_next(std::ptr::null_mut()).is_null());
        assert_eq!(
            fs_xfs_read_file(
                std::ptr::null_mut(),
                p.as_ptr(),
                buf.as_mut_ptr().cast::<c_void>(),
                0,
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
            fs_xfs_read_file(fs, cstr("/small.txt").as_ptr(), std::ptr::null_mut(), 0, 8),
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
            buf.as_mut_ptr().cast::<c_void>(),
            0,
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

// ---------------------------------------------------------------------
// Writing
//
// These work on a copy, because they change it. The copy lives beside
// the fixtures and is removed when the guard drops, including on a
// panic: every other suite here treats each `.img` in `.vm-share` as a
// fixture to check, so one left behind fails unrelated tests.
// ---------------------------------------------------------------------

const EROFS: i32 = 30;
const ENOTSUP: i32 = if cfg!(target_os = "macos") { 45 } else { 95 };

struct WritableCopy(PathBuf);

impl WritableCopy {
    fn new(name: &str) -> Option<Self> {
        let src = fixture()?;
        let dst = src.with_file_name(name);
        std::fs::copy(&src, &dst).ok()?;
        Some(WritableCopy(dst))
    }
    fn open_rw(&self) -> *mut fs_xfs_fs {
        let c = cstr(self.0.to_str().unwrap());
        let fs = unsafe { fs_xfs_mount_rw(c.as_ptr()) };
        assert!(!fs.is_null(), "mount_rw failed: {}", last_error());
        fs
    }
}

impl Drop for WritableCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A read-only handle must report that it cannot write, and must refuse.
///
/// The pairing is the point: a caller that trusts `is_writable` should
/// never be surprised by the refusal, and a caller that ignores it
/// should still be stopped.
#[test]
fn a_read_only_handle_says_so_and_refuses() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    assert_eq!(unsafe { fs_xfs_is_writable(fs) }, 0);

    let data = b"nope";
    let n = unsafe {
        fs_xfs_write_file(
            fs,
            cstr("/large.bin").as_ptr(),
            data.as_ptr().cast::<c_void>(),
            0,
            data.len() as u64,
        )
    };
    assert_eq!(n, -1, "a read-only handle wrote something");
    assert_eq!(fs_xfs_last_errno(), EROFS, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}

#[test]
fn a_read_write_handle_says_so() {
    let Some(copy) = WritableCopy::new("xfscapi-rw.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    assert_eq!(unsafe { fs_xfs_is_writable(fs) }, 1);
    unsafe { fs_xfs_umount(fs) };
}

/// A write through the ABI must be readable back through it.
#[test]
fn a_write_round_trips_through_the_abi() {
    let Some(copy) = WritableCopy::new("xfscapi-write.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let path = cstr("/large.bin");
    let payload = b"written through the C ABI";

    let n = unsafe {
        fs_xfs_write_file(
            fs,
            path.as_ptr(),
            payload.as_ptr().cast::<c_void>(),
            8192,
            payload.len() as u64,
        )
    };
    assert_eq!(n, payload.len() as i64, "{}", last_error());

    let mut back = vec![0u8; payload.len()];
    let r = unsafe {
        fs_xfs_read_file(
            fs,
            path.as_ptr(),
            back.as_mut_ptr().cast::<c_void>(),
            8192,
            back.len() as u64,
        )
    };
    assert_eq!(r, payload.len() as i64, "{}", last_error());
    assert_eq!(
        &back, payload,
        "the bytes read back are not the ones written"
    );
    unsafe { fs_xfs_umount(fs) };
}

/// Writing past the end of a file needs metadata this driver cannot
/// change, and the ABI must say which kind of refusal that is.
#[test]
fn writing_past_the_end_is_enotsup() {
    let Some(copy) = WritableCopy::new("xfscapi-past-end.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let data = b"beyond";
    let n = unsafe {
        fs_xfs_write_file(
            fs,
            cstr("/small.txt").as_ptr(),
            data.as_ptr().cast::<c_void>(),
            1 << 20,
            data.len() as u64,
        )
    };
    assert_eq!(n, -1);
    assert_eq!(fs_xfs_last_errno(), ENOTSUP, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}

/// Truncate through the ABI, and the size visible afterwards.
#[test]
fn a_truncate_is_visible_through_the_abi() {
    let Some(copy) = WritableCopy::new("xfscapi-trunc.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let path = cstr("/large.bin");

    let rc = unsafe { fs_xfs_truncate(fs, path.as_ptr(), 1234, -1, 0) };
    assert_eq!(rc, 0, "{}", last_error());

    let mut st = zeroed_attr();
    assert_eq!(
        unsafe { fs_xfs_stat(fs, path.as_ptr(), &mut st) },
        0,
        "{}",
        last_error()
    );
    assert_eq!(st.size, 1234);
    unsafe { fs_xfs_umount(fs) };
}

/// Growing is refused, and named as unsupported rather than as an error
/// in the arguments.
#[test]
fn growing_by_truncate_is_enotsup() {
    let Some(copy) = WritableCopy::new("xfscapi-grow.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let rc = unsafe { fs_xfs_truncate(fs, cstr("/small.txt").as_ptr(), 1 << 20, -1, 0) };
    assert_eq!(rc, -1);
    assert_eq!(fs_xfs_last_errno(), ENOTSUP, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}

/// Attributes set through the ABI, and read back through it.
#[test]
fn attributes_round_trip_through_the_abi() {
    let Some(copy) = WritableCopy::new("xfscapi-attrs.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let path = cstr("/small.txt");

    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            path.as_ptr(),
            0o640,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            0,
            1_500_000_000,
            42,
        )
    };
    assert_eq!(rc, 0, "{}", last_error());

    let mut st = zeroed_attr();
    assert_eq!(unsafe { fs_xfs_stat(fs, path.as_ptr(), &mut st) }, 0);
    assert_eq!(st.mode & 0o7777, 0o640, "the mode did not take");
    assert_eq!(st.mtime, 1_500_000_000, "the mtime did not take");
    unsafe { fs_xfs_umount(fs) };
}

/// A mode carrying file-type bits must be refused, not masked — this is
/// the ABI's one chance to stop a caller turning a file into a directory
/// by arithmetic.
#[test]
fn a_mode_with_type_bits_is_refused_through_the_abi() {
    let Some(copy) = WritableCopy::new("xfscapi-badmode.img") else {
        eprintln!("no fixture — skipping");
        return;
    };
    let fs = copy.open_rw();
    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            cstr("/small.txt").as_ptr(),
            0o040755,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            0,
            FS_XFS_LEAVE,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_xfs_last_errno(), ENOTSUP, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}
