//! C ABI (`fs_xfs_*`), matching `include/fs_xfs.h`.
//!
//! # Boundary rules
//!
//! Three things must never cross back into C, and each is handled here
//! rather than hoped for:
//!
//! 1. **A panic.** Unwinding into C is undefined behaviour, so every
//!    entry point runs inside [`catch_unwind`] and converts a panic into
//!    the same failure signal any other error produces.
//! 2. **A Rust error type.** Failures become a `-1`/NULL return plus a
//!    thread-local message and errno, which is what a C caller can
//!    actually act on.
//! 3. **A borrowed pointer.** Handles are boxed and leaked deliberately;
//!    the caller owns them until it calls the matching release function.
//!
//! The error state is thread-local, so two threads failing at once do
//! not overwrite each other's message.
//!
//! # Safety contract for callers
//!
//! Pointers must be either NULL or valid for the type named. A handle
//! must not be used after its release function, and must not be used
//! concurrently from two threads. Every function tolerates NULL by
//! failing rather than dereferencing it.

#![allow(non_camel_case_types)]

use crate::dir::DirEntry;
use crate::error::Error;
use crate::fs::Filesystem;
use crate::inode::{FileType, Inode, Timestamp};
use crate::write::AttrChange;
use fs_core::{BlockRead, FileDevice};
use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;

thread_local! {
    /// Message and errno describing this thread's most recent failure.
    static LAST_ERROR: RefCell<(CString, c_int)> =
        RefCell::new((CString::new("no error").unwrap(), 0));
}

/// Map a driver error onto the errno a filesystem client expects.
///
/// The mapping matters: a client distinguishes "this file is not here"
/// from "this volume is damaged" only by the errno, and reporting EIO
/// for a missing file would send a user looking for hardware faults.
fn errno_for(e: &Error) -> c_int {
    match e {
        Error::NotFound => libc_enoent(),
        Error::NotADirectory => libc_enotdir(),
        Error::NotAFile => libc_eisdir(),
        Error::ReadOnly => libc_erofs(),
        Error::UnsupportedFeature(_) => libc_enotsup(),
        // A volume that is not XFS, is malformed, fails a checksum, or
        // holds an unreplayed log is not something the caller can work
        // around; EIO is the honest answer.
        Error::NotXfs { .. }
        | Error::BadSuperblock(_)
        | Error::ChecksumMismatch { .. }
        | Error::BlockIdentityMismatch { .. }
        | Error::DirtyLog
        | Error::Io(_) => libc_eio(),
    }
}

// The errno values are spelled out rather than pulled from a crate, to
// avoid a dependency that exists only for five constants. They are
// identical across Linux and Darwin except where noted.
const fn libc_enoent() -> c_int {
    2
}
const fn libc_eio() -> c_int {
    5
}
const fn libc_eisdir() -> c_int {
    21
}
const fn libc_enotdir() -> c_int {
    20
}
const fn libc_erofs() -> c_int {
    30
}
/// `ERANGE` — a result did not fit the caller's buffer.
const ERANGE: c_int = 34;
/// `ENOTSUP` is 45 on Darwin and 95 on Linux.
const fn libc_enotsup() -> c_int {
    if cfg!(target_os = "macos") {
        45
    } else {
        95
    }
}

fn set_error(message: String, errno: c_int) {
    let c = CString::new(message).unwrap_or_else(|_| CString::new("error").unwrap());
    LAST_ERROR.with(|e| *e.borrow_mut() = (c, errno));
}

fn record(e: &Error) {
    set_error(e.to_string(), errno_for(e));
}

/// Run `f`, converting a panic into a recorded error and `fallback`.
///
/// A panic here means a bug in this crate, not a malformed filesystem —
/// parsers return errors for that. It is still caught, because unwinding
/// into C is undefined behaviour and taking the process down is a worse
/// outcome than an EIO the caller can report.
fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(v) => v,
        Err(_) => {
            set_error("internal error: the driver panicked".into(), libc_eio());
            fallback
        }
    }
}

/// Opaque mounted-filesystem handle.
pub struct fs_xfs_fs {
    fs: Filesystem,
}

/// Opaque directory iterator.
pub struct fs_xfs_dir_iter {
    entries: Vec<DirEntry>,
    next: usize,
    /// Storage for the entry most recently returned.
    ///
    /// `dir_next` hands back a borrowed pointer rather than filling a
    /// caller-supplied struct, matching the sibling drivers. The pointer
    /// stays valid until the next call on the same iterator, which is
    /// what the header promises.
    current: fs_xfs_dirent_t,
}

/// Attributes of one filesystem object.
#[repr(C)]
pub struct fs_xfs_attr_t {
    pub inode: u64,
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: i64,
    pub mtime: i64,
    pub ctime: i64,
    pub crtime: i64,
    pub link_count: u32,
    pub file_type: u32,
}

/// One directory entry.
#[repr(C)]
pub struct fs_xfs_dirent_t {
    pub inode: u64,
    pub file_type: u8,
    pub name_len: u8,
    pub name: [c_char; 256],
}

/// Volume-wide information.
#[repr(C)]
pub struct fs_xfs_volume_info_t {
    pub block_size: u32,
    pub sector_size: u32,
    pub inode_size: u32,
    pub total_blocks: u64,
    pub free_blocks: u64,
    pub inode_count: u64,
    pub free_inodes: u64,
    pub ag_count: u32,
    pub version: u16,
    pub volume_name: [c_char; 13],
    pub uuid: [u8; 16],
    pub feature_compat: u32,
    pub feature_ro_compat: u32,
    pub feature_incompat: u32,
}

/// Read callback for mounting over a caller-supplied device.
pub type fs_xfs_read_fn = Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64) -> c_int>;

/// Caller-supplied block device description.
#[repr(C)]
pub struct fs_xfs_blockdev_cfg_t {
    pub read: fs_xfs_read_fn,
    pub context: *mut c_void,
    pub size_bytes: u64,
    pub block_size: u32,
}

/// Numeric file type shared with the header.
fn file_type_code(t: Option<FileType>) -> u32 {
    match t {
        Some(FileType::Regular) => 1,
        Some(FileType::Directory) => 2,
        Some(FileType::CharDevice) => 3,
        Some(FileType::BlockDevice) => 4,
        Some(FileType::Fifo) => 5,
        Some(FileType::Socket) => 6,
        Some(FileType::Symlink) => 7,
        None => 0,
    }
}

/// Message describing the most recent failure on this thread.
///
/// # Safety
///
/// The returned pointer is valid until the next failing call on this
/// thread. It is never NULL.
#[no_mangle]
pub extern "C" fn fs_xfs_last_error() -> *const c_char {
    LAST_ERROR.with(|e| e.borrow().0.as_ptr())
}

/// POSIX errno for the most recent failure on this thread.
#[no_mangle]
pub extern "C" fn fs_xfs_last_errno() -> c_int {
    LAST_ERROR.with(|e| e.borrow().1)
}

/// Borrow a C string, recording an error and returning `None` if it is
/// NULL or not valid UTF-8.
///
/// # Safety
///
/// `p` must be NULL or point to a NUL-terminated string.
unsafe fn borrow_str<'a>(p: *const c_char, what: &str) -> Option<&'a str> {
    if p.is_null() {
        set_error(format!("{what} is NULL"), libc_enoent());
        return None;
    }
    match unsafe { CStr::from_ptr(p) }.to_str() {
        Ok(s) => Some(s),
        Err(_) => {
            set_error(format!("{what} is not valid UTF-8"), libc_enoent());
            None
        }
    }
}

fn mount_device(device: Arc<dyn BlockRead>) -> *mut fs_xfs_fs {
    match Filesystem::mount(device) {
        Ok(fs) => Box::into_raw(Box::new(fs_xfs_fs { fs })),
        Err(e) => {
            record(&e);
            std::ptr::null_mut()
        }
    }
}

/// Mount the image or device at `device_path`.
///
/// # Safety
///
/// `device_path` must be NULL or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_mount(device_path: *const c_char) -> *mut fs_xfs_fs {
    guard(std::ptr::null_mut(), || {
        let Some(path) = (unsafe { borrow_str(device_path, "device_path") }) else {
            return std::ptr::null_mut();
        };
        match FileDevice::open(path) {
            Ok(dev) => mount_device(Arc::new(dev)),
            Err(e) => {
                set_error(format!("opening {path} failed: {e}"), libc_eio());
                std::ptr::null_mut()
            }
        }
    })
}

/// A block device backed by a C read callback.
struct CallbackDevice {
    read: unsafe extern "C" fn(*mut c_void, *mut c_void, u64, u64) -> c_int,
    context: *mut c_void,
    size: u64,
}

// The caller promises the callback and its context are usable from the
// thread that owns the handle. The handle itself is documented as
// single-threaded, so this is the same contract the C header states.
unsafe impl Send for CallbackDevice {}
unsafe impl Sync for CallbackDevice {}

impl BlockRead for CallbackDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let rc = unsafe {
            (self.read)(
                self.context,
                buf.as_mut_ptr().cast::<c_void>(),
                offset,
                buf.len() as u64,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            // fs_core::Error::Io wraps a std::io::Error, so the callback's
            // return code is reported through one rather than as a bare
            // string. The caller sees its own rc in the message.
            Err(fs_core::Error::Io(std::io::Error::other(format!(
                "the caller's read callback returned {rc} for {} bytes at offset {offset}",
                buf.len()
            ))))
        }
    }

    fn size_bytes(&self) -> u64 {
        self.size
    }
}

/// Mount over a caller-supplied reader.
///
/// # Safety
///
/// `cfg` must be NULL or point to a valid configuration whose `read`
/// callback is safe to call with the given context.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_mount_with_callbacks(
    cfg: *const fs_xfs_blockdev_cfg_t,
) -> *mut fs_xfs_fs {
    guard(std::ptr::null_mut(), || {
        if cfg.is_null() {
            set_error("cfg is NULL".into(), libc_eio());
            return std::ptr::null_mut();
        }
        let cfg = unsafe { &*cfg };
        let Some(read) = cfg.read else {
            set_error("cfg.read is NULL".into(), libc_eio());
            return std::ptr::null_mut();
        };
        mount_device(Arc::new(CallbackDevice {
            read,
            context: cfg.context,
            size: cfg.size_bytes,
        }))
    })
}

/// Mount over an existing `fs_core` device handle.
///
/// This is how the FSKit extension mounts a *partition* rather than a
/// whole disk: the host wraps the block-device resource as an
/// `FsCoreDevice`, slices the partition out of it, and hands the slice
/// here. Without it a caller could only ever mount from offset zero.
///
/// # Safety
///
/// `handle` must be NULL or a live `FsCoreDevice` from `fs_core`.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_mount_with_fs_core_device(
    handle: *mut fs_core::ffi::FsCoreDevice,
) -> *mut fs_xfs_fs {
    guard(std::ptr::null_mut(), || {
        if handle.is_null() {
            set_error("fs_core handle is NULL".into(), libc_eio());
            return std::ptr::null_mut();
        }
        // This driver is read-only, so only the read half of the device
        // trait is needed.
        let dev: std::sync::Arc<dyn fs_core::BlockDevice> = unsafe { (*handle).inner().clone() };
        let read: std::sync::Arc<dyn BlockRead> = dev;
        mount_device(read)
    })
}

/// Release a mounted-filesystem handle.
///
/// # Safety
///
/// `fs` must be NULL or a handle from a successful mount that has not
/// already been released.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_umount(fs: *mut fs_xfs_fs) {
    if fs.is_null() {
        return;
    }
    guard((), || drop(unsafe { Box::from_raw(fs) }));
}

/// # Safety
///
/// `fs` must be a live handle; `out` must be writable.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_get_volume_info(
    fs: *mut fs_xfs_fs,
    out: *mut fs_xfs_volume_info_t,
) -> c_int {
    guard(-1, || {
        if fs.is_null() || out.is_null() {
            set_error("fs or out is NULL".into(), libc_eio());
            return -1;
        }
        let sb = unsafe { &*fs }.fs.superblock();

        let mut name = [0 as c_char; 13];
        for (slot, b) in name.iter_mut().zip(sb.fname.as_bytes()) {
            *slot = *b as c_char;
        }

        unsafe {
            *out = fs_xfs_volume_info_t {
                block_size: sb.blocksize,
                sector_size: u32::from(sb.sectsize),
                inode_size: u32::from(sb.inodesize),
                total_blocks: sb.dblocks,
                free_blocks: sb.fdblocks,
                inode_count: sb.icount,
                free_inodes: sb.ifree,
                ag_count: sb.agcount,
                version: sb.version(),
                volume_name: name,
                uuid: sb.uuid,
                feature_compat: sb.features_compat,
                feature_ro_compat: sb.features_ro_compat,
                feature_incompat: sb.features_incompat,
            };
        }
        0
    })
}

fn fill_attr(inode: &Inode, out: *mut fs_xfs_attr_t) {
    unsafe {
        *out = fs_xfs_attr_t {
            inode: inode.ino,
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            size: inode.size,
            atime: inode.atime.sec,
            mtime: inode.mtime.sec,
            ctime: inode.ctime.sec,
            crtime: inode.crtime.sec,
            link_count: inode.nlink,
            file_type: file_type_code(inode.file_type()),
        };
    }
}

/// # Safety
///
/// `fs` must be a live handle; `path` a NUL-terminated string; `out`
/// writable.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_stat(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    out: *mut fs_xfs_attr_t,
) -> c_int {
    guard(-1, || {
        if fs.is_null() || out.is_null() {
            set_error("fs or out is NULL".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        match unsafe { &*fs }.fs.lookup_path(path) {
            Ok(inode) => {
                fill_attr(&inode, out);
                0
            }
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// # Safety
///
/// `fs` must be a live handle; `out` writable.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_stat_ino(
    fs: *mut fs_xfs_fs,
    inode: u64,
    out: *mut fs_xfs_attr_t,
) -> c_int {
    guard(-1, || {
        if fs.is_null() || out.is_null() {
            set_error("fs or out is NULL".into(), libc_eio());
            return -1;
        }
        match unsafe { &*fs }.fs.read_inode(inode) {
            Ok(i) => {
                fill_attr(&i, out);
                0
            }
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// Open a directory for iteration.
///
/// The whole listing is materialised up front. A directory large enough
/// for that to matter would need a streaming iterator holding a borrow
/// of the filesystem across the C boundary, which is a lifetime this ABI
/// cannot express safely.
///
/// # Safety
///
/// `fs` must be a live handle; `path` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_dir_open(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
) -> *mut fs_xfs_dir_iter {
    guard(std::ptr::null_mut(), || {
        if fs.is_null() {
            set_error("fs is NULL".into(), libc_eio());
            return std::ptr::null_mut();
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return std::ptr::null_mut();
        };
        match unsafe { &*fs }.fs.list_path(path) {
            Ok(entries) => Box::into_raw(Box::new(fs_xfs_dir_iter {
                entries,
                next: 0,
                current: unsafe { std::mem::zeroed() },
            })),
            Err(e) => {
                record(&e);
                std::ptr::null_mut()
            }
        }
    })
}

/// # Safety
///
/// `iter` must be a live iterator; `out` writable.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_dir_next(iter: *mut fs_xfs_dir_iter) -> *const fs_xfs_dirent_t {
    guard(std::ptr::null(), || {
        if iter.is_null() {
            set_error("iter is NULL".into(), libc_eio());
            return std::ptr::null();
        }
        let it = unsafe { &mut *iter };
        let Some(e) = it.entries.get(it.next) else {
            return std::ptr::null();
        };
        it.next += 1;

        // The name field is fixed at 256 bytes and must stay
        // NUL-terminated, so a longer name is truncated rather than
        // overrunning. XFS caps names at 255 bytes, so this only trims
        // the terminator's worth in the pathological case.
        let mut name = [0 as c_char; 256];
        let n = e.name.len().min(255);
        for (slot, b) in name.iter_mut().zip(&e.name[..n]) {
            *slot = *b as c_char;
        }
        it.current = fs_xfs_dirent_t {
            inode: e.ino,
            file_type: file_type_code(e.ftype) as u8,
            name_len: n as u8,
            name,
        };
        &it.current
    })
}

/// # Safety
///
/// `iter` must be NULL or a live iterator not already released.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_dir_close(iter: *mut fs_xfs_dir_iter) {
    if iter.is_null() {
        return;
    }
    guard((), || drop(unsafe { Box::from_raw(iter) }));
}

/// # Safety
///
/// `fs` must be a live handle; `path` NUL-terminated; `buf` writable for
/// `length` bytes.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_read_file(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    buf: *mut c_void,
    offset: u64,
    length: u64,
) -> i64 {
    guard(-1, || {
        if fs.is_null() || buf.is_null() {
            set_error("fs or buf is NULL".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        let fs = &unsafe { &*fs }.fs;

        let found = match fs.lookup_path(path) {
            Ok(i) => i,
            Err(e) => {
                record(&e);
                return -1;
            }
        };
        let (inode, raw) = match fs.read_inode_raw(found.ino) {
            Ok(v) => v,
            Err(e) => {
                record(&e);
                return -1;
            }
        };
        let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), length as usize) };
        match fs.read_at(&inode, &raw, offset, out) {
            Ok(n) => n as i64,
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// # Safety
///
/// `fs` must be a live handle; `path` NUL-terminated; `buf` writable for
/// `bufsize` bytes.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_readlink(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    buf: *mut c_char,
    bufsize: usize,
) -> c_int {
    guard(-1, || {
        if fs.is_null() || buf.is_null() || bufsize == 0 {
            set_error("fs or buf is NULL, or bufsize is zero".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        let fs = &unsafe { &*fs }.fs;

        let found = match fs.lookup_path(path) {
            Ok(i) => i,
            Err(e) => {
                record(&e);
                return -1;
            }
        };
        let (inode, raw) = match fs.read_inode_raw(found.ino) {
            Ok(v) => v,
            Err(e) => {
                record(&e);
                return -1;
            }
        };
        match fs.read_link(&inode, &raw) {
            Ok(target) => {
                // Refuse rather than truncate. A truncated symlink target
                // is a path to somewhere else, and a caller following it
                // has no way to tell — so a buffer that cannot hold the
                // whole target plus its terminator is an error, not a
                // partial success. ERANGE tells the caller to retry with
                // a larger buffer, which is the standard idiom.
                //
                // The sibling EROFS driver already behaved this way; the
                // family now agrees.
                if target.len() + 1 > bufsize {
                    set_error(
                        format!(
                            "readlink buffer holds {bufsize} bytes, need {} for the target \
                             and its terminator",
                            target.len() + 1
                        ),
                        ERANGE,
                    );
                    return -1;
                }
                let out = unsafe { std::slice::from_raw_parts_mut(buf.cast::<u8>(), bufsize) };
                out[..target.len()].copy_from_slice(&target);
                out[target.len()] = 0;
                target.len() as c_int
            }
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

// ---------------------------------------------------------------------
// Writing
//
// Every entry point here needs a handle opened by `fs_xfs_mount_rw`.
// One opened by `fs_xfs_mount` refuses with EROFS, so a caller cannot
// write to a volume by accident — the decision is made once, when the
// filesystem is opened, rather than at each call.
// ---------------------------------------------------------------------

/// Mount the image or device at `device_path` for reading **and
/// writing**.
///
/// Returns NULL if the device cannot be written, if the volume's log
/// holds unapplied records, or for any reason [`fs_xfs_mount`] would.
///
/// # Safety
///
/// `device_path` must be NULL or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_mount_rw(device_path: *const c_char) -> *mut fs_xfs_fs {
    guard(std::ptr::null_mut(), || {
        let Some(path) = (unsafe { borrow_str(device_path, "device_path") }) else {
            return std::ptr::null_mut();
        };
        match FileDevice::open_rw(path) {
            Ok(dev) => match Filesystem::mount_rw(Arc::new(dev)) {
                Ok(fs) => Box::into_raw(Box::new(fs_xfs_fs { fs })),
                Err(e) => {
                    record(&e);
                    std::ptr::null_mut()
                }
            },
            Err(e) => {
                set_error(
                    format!("opening {path} for writing failed: {e}"),
                    libc_eio(),
                );
                std::ptr::null_mut()
            }
        }
    })
}

/// Whether this handle can write.
///
/// Lets a caller ask rather than discover: presenting a volume as
/// writable and then failing every write is worse than knowing up front.
///
/// # Safety
///
/// `fs` must be a live handle or NULL.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_is_writable(fs: *mut fs_xfs_fs) -> c_int {
    guard(0, || {
        if fs.is_null() {
            return 0;
        }
        c_int::from(unsafe { &*fs }.fs.is_writable())
    })
}

/// Overwrite `length` bytes of an existing file at `offset`.
///
/// Returns the number of bytes written, or −1 with the error recorded.
/// The whole range is written or none of it is; a short write is not
/// possible.
///
/// Only bytes that already exist can be rewritten. Writing past the end
/// of the file, into a hole, or into an extent that was allocated but
/// never written is refused with ENOTSUP, because each would change the
/// filesystem's metadata and this driver has no journal to do that
/// safely.
///
/// # Safety
///
/// `fs` must be a live handle; `path` NUL-terminated; `buf` readable for
/// `length` bytes.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_write_file(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    buf: *const c_void,
    offset: u64,
    length: u64,
) -> i64 {
    guard(-1, || {
        if fs.is_null() || buf.is_null() {
            set_error("fs or buf is NULL".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        let fs = &unsafe { &*fs }.fs;
        let Some((inode, raw)) = resolve_for_write(fs, path) else {
            return -1;
        };
        let data = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), length as usize) };
        match fs.write_at(&inode, &raw, offset, data) {
            Ok(n) => n as i64,
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// Shorten a file to `new_size`.
///
/// Returns 0, or −1 with the error recorded. Growing is refused with
/// ENOTSUP: it needs blocks that are not allocated.
///
/// The file's blocks are **not** released. Freeing them is allocation
/// work that needs a journal, so a shortened file still occupies what it
/// did before.
///
/// `mtime_sec` and `mtime_nsec` set the modification and inode-change
/// times; pass a negative `mtime_sec` to leave both alone.
///
/// # Safety
///
/// `fs` must be a live handle and `path` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_truncate(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    new_size: u64,
    mtime_sec: i64,
    mtime_nsec: u32,
) -> c_int {
    guard(-1, || {
        if fs.is_null() {
            set_error("fs is NULL".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        let fs = &unsafe { &*fs }.fs;
        let Some((inode, _)) = resolve_for_write(fs, path) else {
            return -1;
        };
        let when = (mtime_sec >= 0).then_some(Timestamp {
            sec: mtime_sec,
            nsec: mtime_nsec,
        });
        match fs.truncate(&inode, new_size, when) {
            Ok(()) => 0,
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// Sentinel meaning "leave this field alone" for the setters below.
///
/// A negative value cannot be a real time, uid, gid or mode, so one
/// signed parameter can carry both "set it to this" and "do not touch
/// it" without a second flags argument for the caller to get wrong.
pub const FS_XFS_LEAVE: i64 = -1;

/// Change a file's timestamps, permissions or ownership.
///
/// Each parameter is either a value to set or [`FS_XFS_LEAVE`]. Fields
/// left alone keep whatever they hold, including anything changed by
/// something else since the caller last looked.
///
/// `mode` takes permission bits only. A value with file-type bits set is
/// refused with ENOTSUP rather than silently masked: changing a file
/// into a directory is not an attribute change.
///
/// Returns 0, or −1 with the error recorded.
///
/// # Safety
///
/// `fs` must be a live handle and `path` NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn fs_xfs_set_attributes(
    fs: *mut fs_xfs_fs,
    path: *const c_char,
    mode: i64,
    uid: i64,
    gid: i64,
    atime_sec: i64,
    atime_nsec: u32,
    mtime_sec: i64,
    mtime_nsec: u32,
) -> c_int {
    guard(-1, || {
        if fs.is_null() {
            set_error("fs is NULL".into(), libc_eio());
            return -1;
        }
        let Some(path) = (unsafe { borrow_str(path, "path") }) else {
            return -1;
        };
        let fs = &unsafe { &*fs }.fs;
        let Some((inode, _)) = resolve_for_write(fs, path) else {
            return -1;
        };
        let change = AttrChange {
            permissions: (mode >= 0).then_some(mode as u16),
            uid: (uid >= 0).then_some(uid as u32),
            gid: (gid >= 0).then_some(gid as u32),
            atime: (atime_sec >= 0).then_some(Timestamp {
                sec: atime_sec,
                nsec: atime_nsec,
            }),
            mtime: (mtime_sec >= 0).then_some(Timestamp {
                sec: mtime_sec,
                nsec: mtime_nsec,
            }),
            ctime: None,
        };
        match fs.set_attributes(&inode, &change) {
            Ok(()) => 0,
            Err(e) => {
                record(&e);
                -1
            }
        }
    })
}

/// Resolve a path to the inode and its bytes, recording any error.
///
/// Shared by the write entry points so a failure to find the file is
/// reported the same way whichever one a caller reached for.
fn resolve_for_write(fs: &Filesystem, path: &str) -> Option<(Inode, Vec<u8>)> {
    let found = match fs.lookup_path(path) {
        Ok(i) => i,
        Err(e) => {
            record(&e);
            return None;
        }
    };
    match fs.read_inode_raw(found.ino) {
        Ok(v) => Some(v),
        Err(e) => {
            record(&e);
            None
        }
    }
}
