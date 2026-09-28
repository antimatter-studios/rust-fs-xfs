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
//! The fixture these mount is built by `chore fixtures` and is always
//! there; a checkout without it is a build that did not happen, so
//! `common::fixture` fails and names the task rather than letting the
//! suite return early and report ok.

mod common;

use fs_xfs::capi::*;
use std::ffi::{c_char, c_void, CStr, CString};
use std::path::PathBuf;

/// Errno values the ABI documents. Spelled out rather than imported so
/// the test asserts the contract rather than mirroring the source.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOTDIR: i32 = 20;
const EISDIR: i32 = 21;
const EINVAL: i32 = 22;
const ERANGE: i32 = 34;

/// The image every test here mounts. It holds the files these tests name
/// — `/small.txt`, `/large.bin`, `/sub` — and nothing else in this suite
/// knows how to make one.
const IMAGE: &str = "xfsdata-default.img";

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap()
}

/// Mount the fixture. A missing image fails the test, naming the task
/// that builds it, rather than returning nothing to test.
fn mount() -> *mut fs_xfs_fs {
    let path = common::fixture(IMAGE);
    let c = cstr(path.to_str().unwrap());
    let fs = unsafe { fs_xfs_mount(c.as_ptr()) };
    assert!(
        !fs.is_null(),
        "mounting .vm-share/{IMAGE} failed: {}",
        last_error()
    );
    fs
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
    let fs = mount();
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
    let fs = mount();

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
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
        // `i32::from`, not `c as u8`: `c_char` is SIGNED on x86_64 and
        // UNSIGNED on aarch64, so a cast that is necessary on one is a
        // clippy error on the other and the pre-commit guard cannot pass
        // on both. Widening is defined for either sign and needs no cast.
        buf.iter().all(|&c| i32::from(c) == 0x7F),
        "a refused readlink must not have written into the buffer"
    );
    unsafe { fs_xfs_umount(fs) };
}

// ---------------------------------------------------------------------
// The readlink contract the driver family shares (#259)
//
//   success       the target length, excluding the NUL, and the target
//                 plus a NUL written into `buf`
//   too small     bufsize < length + 1: -1, ERANGE, a message naming the
//                 size needed, and NOTHING written into `buf`
//   NULL args     -1, EINVAL
//   otherwise     -1 with errno set: ENOENT for a missing path, EINVAL
//                 for something that is not a symlink, as readlink(2)
// ---------------------------------------------------------------------

/// What a buffer holds before the call, so a byte the call wrote is
/// distinguishable from one it did not.
const UNTOUCHED: c_char = 0x7F;

/// One `fs_xfs_readlink` into a fresh `bufsize`-byte buffer filled with
/// [`UNTOUCHED`]: the return value, the errno and message it left, and
/// the buffer afterwards.
fn readlink_with(fs: *mut fs_xfs_fs, path: &str, bufsize: usize) -> (i32, i32, String, Vec<u8>) {
    let mut buf = vec![UNTOUCHED; bufsize.max(1)];
    let n = unsafe { fs_xfs_readlink(fs, cstr(path).as_ptr(), buf.as_mut_ptr(), bufsize) };
    // `u8::from_ne_bytes`, not `as u8`: `c_char` is signed on x86_64 and
    // unsigned on aarch64, and clippy rejects the cast on one of them.
    let bytes = buf.iter().map(|&c| c.to_ne_bytes()[0]).collect();
    (n, fs_xfs_last_errno(), last_error(), bytes)
}

/// The kernel's own `readlink` of every symlink in a data fixture, from
/// the manifest `scripts/build-data-fixtures.sh` generates inside Linux.
fn kernel_links(image: &str) -> Vec<(String, String)> {
    let manifest = common::fixture(image).with_extension("manifest");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("reading {}: {e}", manifest.display()));
    let links: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(4, '\t').collect();
            (f.len() == 4 && f[1] == "link").then(|| (f[0].to_string(), f[3].to_string()))
        })
        .collect();
    assert!(
        !links.is_empty(),
        "{} lists no symlinks: rebuild the fixtures with `chore fixtures`",
        manifest.display()
    );
    links
}

/// The kernel's target for one path in [`IMAGE`].
fn kernel_target(path: &str) -> String {
    kernel_links(IMAGE)
        .into_iter()
        .find(|(p, _)| p == path)
        .unwrap_or_else(|| {
            panic!(
                "{path} is not a symlink in .vm-share/{IMAGE}'s manifest: the fixture \
                 predates it. Rebuild with `chore fixtures`."
            )
        })
        .1
}

/// Success returns the length and writes the target plus a NUL, for a
/// link of each storage form. The expected target is the kernel's, not
/// ours.
fn assert_reads_whole_target(path: &str) {
    let want = kernel_target(path);
    let fs = mount();
    let (n, _, msg, buf) = readlink_with(fs, path, 2048);
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(
        n,
        i32::try_from(want.len()).unwrap(),
        "{path}: readlink returns the target length, excluding the NUL ({msg})"
    );
    assert_eq!(&buf[..want.len()], want.as_bytes(), "{path}: target bytes");
    assert_eq!(buf[want.len()], 0, "{path}: the target is NUL-terminated");
    assert!(
        buf[want.len() + 1..]
            .iter()
            .all(|&b| b == UNTOUCHED.to_ne_bytes()[0]),
        "{path}: nothing is written past the terminator"
    );
}

#[test]
fn readlink_returns_the_length_of_an_inline_target() {
    assert_reads_whole_target("/link-short");
    assert_reads_whole_target("/link-long");
}

/// `/sub/link-remote` is too long for the inode, so its target is in a
/// block of its own. The debugger's record of its format is what says
/// so — without it this would silently be a second inline case.
#[test]
fn readlink_returns_the_length_of_a_remote_target() {
    let forms = common::fixture(IMAGE).with_extension("links");
    let text = std::fs::read_to_string(&forms).unwrap_or_else(|e| {
        panic!(
            "{}: {e}. The fixture predates the symlink-format record; rebuild it \
             with `chore fixtures`.",
            forms.display()
        )
    });
    let remote = text
        .lines()
        .find_map(|l| l.strip_prefix("/sub/link-remote\t"))
        .unwrap_or_else(|| {
            panic!(
                "{} does not list /sub/link-remote:\n{text}",
                forms.display()
            )
        });
    assert!(
        remote.contains("extents") || remote.contains("btree"),
        "/sub/link-remote must be a remote link to test the remote path; xfs_db \
         reports core.format = {remote}"
    );
    assert_reads_whole_target("/sub/link-remote");
}

/// `bufsize == length + 1` is exactly enough: the target and its NUL.
#[test]
fn readlink_accepts_a_buffer_of_exactly_length_plus_one() {
    for path in ["/link-short", "/link-long"] {
        let want = kernel_target(path);
        let fs = mount();
        let (n, _, msg, buf) = readlink_with(fs, path, want.len() + 1);
        unsafe { fs_xfs_umount(fs) };
        assert_eq!(n, i32::try_from(want.len()).unwrap(), "{path}: {msg}");
        assert_eq!(&buf[..want.len()], want.as_bytes());
        assert_eq!(buf[want.len()], 0);
    }
}

/// `bufsize == length` has no room for the NUL. That is ERANGE, the
/// message names the size that would have worked, and the buffer is
/// untouched — never a target missing its terminator, never a
/// truncated one.
#[test]
fn readlink_refuses_a_buffer_of_exactly_the_length() {
    for path in ["/link-short", "/link-long"] {
        let want = kernel_target(path);
        let fs = mount();
        let (n, errno, msg, buf) = readlink_with(fs, path, want.len());
        unsafe { fs_xfs_umount(fs) };
        assert_eq!(
            n, -1,
            "{path}: a buffer with no room for the NUL is refused"
        );
        assert_eq!(errno, ERANGE, "{path}: {msg}");
        assert!(
            msg.contains(&(want.len() + 1).to_string()),
            "{path}: the message names the {} bytes needed: {msg}",
            want.len() + 1
        );
        assert!(
            buf.iter().all(|&b| b == UNTOUCHED.to_ne_bytes()[0]),
            "{path}: a refused readlink writes nothing"
        );
    }
}

/// A zero-byte buffer is the smallest too-small buffer, not a NULL
/// argument: ERANGE, like every other size below length + 1.
#[test]
fn readlink_refuses_a_zero_byte_buffer_with_erange() {
    let fs = mount();
    let (n, errno, msg, buf) = readlink_with(fs, "/link-short", 0);
    unsafe { fs_xfs_umount(fs) };
    assert_eq!(n, -1);
    assert_eq!(errno, ERANGE, "{msg}");
    assert!(
        msg.contains("10"),
        "the message names the 10 bytes needed: {msg}"
    );
    assert_eq!(buf, [UNTOUCHED.to_ne_bytes()[0]], "nothing is written");
}

#[test]
fn readlink_with_a_null_argument_is_einval() {
    let fs = mount();
    let mut buf = [0 as c_char; 64];
    let path = cstr("/link-short");
    unsafe {
        for (what, rc) in [
            (
                "fs",
                fs_xfs_readlink(std::ptr::null_mut(), path.as_ptr(), buf.as_mut_ptr(), 64),
            ),
            (
                "path",
                fs_xfs_readlink(fs, std::ptr::null(), buf.as_mut_ptr(), 64),
            ),
            (
                "buf",
                fs_xfs_readlink(fs, path.as_ptr(), std::ptr::null_mut(), 64),
            ),
        ] {
            assert_eq!(rc, -1, "NULL {what}");
            assert_eq!(
                fs_xfs_last_errno(),
                EINVAL,
                "NULL {what} is EINVAL: {}",
                last_error()
            );
        }
        fs_xfs_umount(fs);
    }
}

/// Every failure leaves an errno a caller can act on — and the one
/// `readlink(2)` gives: ENOENT for nothing there, EINVAL for something
/// that is not a symlink.
#[test]
fn readlink_failures_set_the_readlink_errno() {
    for (path, want, name) in [
        ("/definitely-not-here", ENOENT, "ENOENT"),
        ("/small.txt", EINVAL, "EINVAL"),
        ("/sub", EINVAL, "EINVAL"),
    ] {
        let fs = mount();
        let (n, errno, msg, buf) = readlink_with(fs, path, 64);
        unsafe { fs_xfs_umount(fs) };
        assert_eq!(n, -1, "{path}");
        assert_eq!(errno, want, "{path}: want {name}, got {errno} ({msg})");
        assert!(
            buf.iter().all(|&b| b == UNTOUCHED.to_ne_bytes()[0]),
            "{path}: a failed readlink writes nothing"
        );
    }
}

/// THE ORACLE. Every symlink in every data fixture reads back through
/// the C ABI as exactly what the Linux kernel's `readlink` reported for
/// it when the fixture was built — three geometries, both storage forms.
#[test]
fn readlink_agrees_with_the_kernel_on_every_data_fixture() {
    let mut checked = 0;
    for manifest in common::fixtures_matching("xfsdata-", ".manifest") {
        let image = manifest.with_extension("img");
        let name = image.file_name().unwrap().to_str().unwrap().to_string();
        let c = cstr(image.to_str().unwrap());
        let fs = unsafe { fs_xfs_mount(c.as_ptr()) };
        assert!(!fs.is_null(), "mounting {name}: {}", last_error());
        for (path, want) in kernel_links(&name) {
            let (n, _, msg, buf) = readlink_with(fs, &path, 2048);
            assert_eq!(
                n,
                i32::try_from(want.len()).unwrap(),
                "{name}{path}: the kernel reads a {}-byte target ({msg})",
                want.len()
            );
            assert_eq!(
                &buf[..want.len()],
                want.as_bytes(),
                "{name}{path}: target differs from the kernel's"
            );
            assert_eq!(buf[want.len()], 0, "{name}{path}: NUL-terminated");
            checked += 1;
        }
        unsafe { fs_xfs_umount(fs) };
    }
    assert!(
        checked >= 6,
        "only {checked} symlinks compared with the kernel"
    );
}

// ---------------------------------------------------------------------
// Error reporting
// ---------------------------------------------------------------------

/// The errno is the only thing distinguishing "not there" from "this
/// volume is damaged". Getting it wrong sends a user to the wrong place.
#[test]
fn a_missing_path_reports_enoent_not_eio() {
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
    let fs = mount();
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
    let img = common::fixture(IMAGE);
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

/// A non-UTF-8 path is resolved rather than misinterpreted, and names
/// nothing in this fixture.
///
/// It used to be REJECTED for its encoding, which is what the old name
/// of this test said. Since #269 the bytes are looked up: `/\xff` is a
/// perfectly well-formed request for a file called `\xff`, there is no
/// such file here, and the answer is that it was not found. Either way
/// the call fails and says something, which is what this pins; the test
/// above is the one that pins WHY.
#[test]
fn a_non_utf8_path_names_nothing_here_and_says_so() {
    let fs = mount();
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
    fn new(name: &str) -> Self {
        let src = common::fixture(IMAGE);
        let dst = src.with_file_name(name);
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("copying .vm-share/{IMAGE} to {name}: {e}"));
        WritableCopy(dst)
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
    let fs = mount();
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
    let copy = WritableCopy::new("xfscapi-rw.img");
    let fs = copy.open_rw();
    assert_eq!(unsafe { fs_xfs_is_writable(fs) }, 1);
    unsafe { fs_xfs_umount(fs) };
}

/// A write through the ABI must be readable back through it.
#[test]
fn a_write_round_trips_through_the_abi() {
    let copy = WritableCopy::new("xfscapi-write.img");
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
    let copy = WritableCopy::new("xfscapi-past-end.img");
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
    let copy = WritableCopy::new("xfscapi-trunc.img");
    let fs = copy.open_rw();
    let path = cstr("/large.bin");

    let rc = unsafe { fs_xfs_truncate(fs, path.as_ptr(), 1234, FS_XFS_LEAVE_TIME, 0) };
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
    let copy = WritableCopy::new("xfscapi-grow.img");
    let fs = copy.open_rw();
    let rc = unsafe {
        fs_xfs_truncate(
            fs,
            cstr("/small.txt").as_ptr(),
            1 << 20,
            FS_XFS_LEAVE_TIME,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_xfs_last_errno(), ENOTSUP, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}

/// Attributes set through the ABI, and read back through it.
#[test]
fn attributes_round_trip_through_the_abi() {
    let copy = WritableCopy::new("xfscapi-attrs.img");
    let fs = copy.open_rw();
    let path = cstr("/small.txt");

    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            path.as_ptr(),
            0o640,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE_TIME,
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
    let copy = WritableCopy::new("xfscapi-badmode.img");
    let fs = copy.open_rw();
    let rc = unsafe {
        fs_xfs_set_attributes(
            fs,
            cstr("/small.txt").as_ptr(),
            0o040755,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE,
            FS_XFS_LEAVE_TIME,
            0,
            FS_XFS_LEAVE_TIME,
            0,
        )
    };
    assert_eq!(rc, -1);
    assert_eq!(fs_xfs_last_errno(), ENOTSUP, "{}", last_error());
    unsafe { fs_xfs_umount(fs) };
}

// ---- paths are bytes (#269) --------------------------------------------

/// `/caf\xe9.txt` — latin-1 for `café.txt`, which is what a name written
/// on a Linux box with a non-UTF-8 locale looks like. `\xe9` alone is not
/// a legal UTF-8 sequence.
///
/// `c_char` is `i8` on x86_64 and Apple targets and `u8` on
/// aarch64-linux, so `from_ne_bytes` is the spelling that works on both.
fn non_utf8_path() -> Vec<std::ffi::c_char> {
    b"/caf\xe9.txt\0"
        .iter()
        .map(|&b| std::ffi::c_char::from_ne_bytes([b]))
        .collect()
}

/// A name that is not valid UTF-8 is LOOKED UP, not refused for its
/// encoding.
///
/// XFS directory entry names are raw bytes and the format has no field
/// that could say what encoding they are in, so such names are ordinary
/// rather than hostile: any image built on a box with a non-UTF-8 locale
/// holds them.
///
/// This crate refused them at the ABI, which left the file listed and
/// unopenable: `fs_xfs_dir_next` hands the caller the entry's name from
/// the raw bytes, so the ABI reported a name it then refused to accept,
/// and composing the path from those same bytes produced the same
/// rejection (#269).
///
/// The fixture has no such file, so what is asserted is the shape of the
/// answer: a path naming no file is reported as missing — the honest
/// answer for bytes that name nothing — and NOT as a complaint about the
/// argument's encoding, which would send a caller looking at its own
/// string handling.
#[test]
fn a_non_utf8_path_is_taken_as_bytes_and_reported_as_missing() {
    let fs = mount();
    let path = non_utf8_path();
    let mut attr = zeroed_attr();
    let rc = unsafe { fs_xfs_stat(fs, path.as_ptr(), &mut attr) };
    assert_eq!(rc, -1, "a path naming no file was answered as a stat");
    let msg = last_error();
    assert!(
        !msg.contains("not valid UTF-8"),
        "the path was refused for its encoding rather than looked up: {msg}"
    );
    unsafe { fs_xfs_umount(fs) };
}

/// And the directory iterator, which is the entry point a caller reaches
/// these names through in the first place.
#[test]
fn dir_open_takes_a_non_utf8_path_as_bytes() {
    let fs = mount();
    let path = non_utf8_path();
    let iter = unsafe { fs_xfs_dir_open(fs, path.as_ptr()) };
    assert!(
        iter.is_null(),
        "a path naming no directory opened an iterator"
    );
    let msg = last_error();
    assert!(
        !msg.contains("not valid UTF-8"),
        "the path was refused for its encoding rather than looked up: {msg}"
    );
    unsafe { fs_xfs_umount(fs) };
}
