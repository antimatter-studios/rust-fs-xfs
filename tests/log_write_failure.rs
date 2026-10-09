//! A mount stops writing after a log write fails (#400).
//!
//! A journalled operation writes its record into the log and flushes it.
//! When that fails, the record may be wholly on the device, partly, or
//! not at all, and this mount cannot tell which. Writing anything after
//! it would build on a log head that may be wrong: the next record would
//! go where the failed one started, over blocks that may hold part of it
//! with the current cycle number, and recovery could read the remains as
//! log.
//!
//! The kernel answers a log I/O error by shutting the filesystem down
//! (`xfs_force_shutdown(SHUTDOWN_LOG_IO_ERROR)`), and every later change
//! fails with EIO. So does this driver: after one failed log write, every
//! mutating call on the same mount fails with [`Error::Io`], and the
//! device is not written again. A fresh mount reads whatever the log
//! holds, as after any crash.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on a sparse in-memory device that can fail one
//! flush. What the kernel makes of the result is
//! `tests/torn_checkpoint_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: u64 = 512;

/// A device that keeps only the sectors written to it, can fail its next
/// flush once, and counts every write after that failure.
struct Sparse {
    sectors: Mutex<BTreeMap<u64, Vec<u8>>>,
    fail_next_flush: AtomicBool,
    failed: AtomicBool,
    writes_after_failure: AtomicU64,
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = sectors
                .get(&(at / SECTOR))
                .map_or(0, |s| s[(at % SECTOR) as usize]);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        if self.failed.load(Ordering::SeqCst) {
            self.writes_after_failure.fetch_add(1, Ordering::SeqCst);
        }
        let mut sectors = self.sectors.lock().unwrap();
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            sectors
                .entry(at / SECTOR)
                .or_insert_with(|| vec![0; SECTOR as usize])[(at % SECTOR) as usize] = byte;
        }
        Ok(())
    }
    fn flush(&self) -> fs_core::Result<()> {
        if self.fail_next_flush.swap(false, Ordering::SeqCst) {
            self.failed.store(true, Ordering::SeqCst);
            return Err(fs_core::Error::Io(std::io::Error::other(
                "injected flush failure",
            )));
        }
        if self.failed.load(Ordering::SeqCst) {
            self.writes_after_failure.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A formatted volume mounted read-write, and its device.
fn mounted() -> (Arc<Sparse>, Filesystem) {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
        fail_next_flush: AtomicBool::new(false),
        failed: AtomicBool::new(false),
        writes_after_failure: AtomicU64::new(0),
    });
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (dev, fs)
}

/// The first journalled change after mount fails its flush.
fn fail_one_log_write(dev: &Sparse, fs: &Filesystem, root: u64) {
    dev.fail_next_flush.store(true, Ordering::SeqCst);
    let failed = fs.create_file(root, b"torn", 0o100644);
    assert!(
        dev.failed.load(Ordering::SeqCst),
        "the create never flushed, so nothing failed: {failed:?}"
    );
    assert!(
        failed.is_err(),
        "a create whose log flush failed reported success"
    );
}

fn is_shut_down(result: &Result<impl std::fmt::Debug, Error>) -> bool {
    matches!(result, Err(Error::Io(m)) if m.contains("log write failed"))
}

#[test]
fn every_change_after_a_failed_log_write_is_refused_and_writes_nothing() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    // Something to change afterwards, made before anything fails.
    let (file, _) = fs.create_file(root, b"before", 0o100644).expect("create");
    fail_one_log_write(&dev, &fs, root);

    let create = fs.create_file(root, b"after", 0o100644);
    assert!(
        is_shut_down(&create),
        "create after the failure: {create:?}"
    );
    let unlink = fs.unlink_file(root, b"before");
    assert!(
        is_shut_down(&unlink),
        "unlink after the failure: {unlink:?}"
    );
    let rename = fs.rename_in_directory(root, b"before", b"renamed");
    assert!(
        is_shut_down(&rename),
        "rename after the failure: {rename:?}"
    );
    let fill = fs.write_into_empty_file(file, b"data");
    assert!(is_shut_down(&fill), "write after the failure: {fill:?}");
    let truncate = fs.truncate_to_zero(file);
    assert!(
        is_shut_down(&truncate),
        "truncate after the failure: {truncate:?}"
    );
    let label = fs.set_label("after");
    assert!(
        is_shut_down(&label),
        "set_label after the failure: {label:?}"
    );
    let sync = fs.sync();
    assert!(is_shut_down(&sync), "sync after the failure: {sync:?}");

    assert_eq!(
        dev.writes_after_failure.load(Ordering::SeqCst),
        0,
        "the mount wrote to the device after its log write failed"
    );
}

#[test]
fn reads_still_work_after_a_failed_log_write() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    fs.create_file(root, b"before", 0o100644).expect("create");
    fail_one_log_write(&dev, &fs, root);
    fs.lookup_path("/before")
        .expect("a mount that stopped writing still reads");
}

#[test]
fn a_fresh_mount_after_the_failure_writes_again() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    fail_one_log_write(&dev, &fs, root);
    drop(fs);
    // What the failed mount left is a log a crash could have left; a new
    // read-write mount refuses it as dirty or takes it as clean, and
    // either way is not shut down by the old mount's failure.
    match Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>) {
        Ok(fresh) => {
            let made = fresh.create_file(root, b"again", 0o100644);
            assert!(
                !is_shut_down(&made),
                "a fresh mount inherited the shutdown: {made:?}"
            );
        }
        Err(Error::DirtyLog) => {}
        Err(other) => panic!("a fresh mount after the failure: {other:?}"),
    }
}
