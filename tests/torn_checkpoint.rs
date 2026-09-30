//! A checkpoint whose write failed is not written over (#92).
//!
//! PROBE — reports what the kernel makes of a mount that carried on
//! after one of its log writes failed.

mod common;

use common::{kernel_run, repair, scratch, share};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

const SUITE: &str = "torn_checkpoint";
const PIECES: u32 = 200;

/// A file device whose next flush fails once armed: the bytes are
/// written, and the caller is told they may not be on stable storage.
struct FailingFlush {
    inner: FileDevice,
    armed: AtomicBool,
}

impl BlockRead for FailingFlush {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

impl BlockDevice for FailingFlush {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> fs_core::Result<()> {
        if self.armed.swap(false, Ordering::SeqCst) {
            return Err(fs_core::Error::Io(std::io::Error::other(
                "injected flush failure",
            )));
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

#[test]
fn probe() {
    assert!(share().is_dir(), "no .vm-share");
    let volume = scratch::Volume::empty(SUITE, "torn.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        for i in $(seq 0 {last}); do
            dd if=/dev/zero of="$m/gone" bs=4096 count=1 seek=$((i * 2)) conv=notrunc status=none
            dd if=/dev/zero of="$m/kept" bs=4096 count=1 seek=$((i * 2 + 1)) conv=notrunc status=none
        done
        sync
        echo "GONE_EXTENTS $(xfs_bmap "$m/gone" | grep -c ':')"
        umount "$m"; rmdir "$m"
        echo DONE
        "#,
        last = PIECES - 1,
    ));
    eprintln!("{built}");
    assert!(built.contains("MOUNT_OK"));

    let dev = Arc::new(FailingFlush {
        inner: FileDevice::open_rw(volume.path().to_str().unwrap()).unwrap(),
        armed: AtomicBool::new(false),
    });
    {
        let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.lookup_path("/").unwrap().ino;
        fs.create_file(root, b"zero", 0o100644)
            .expect("first create");
        let gone = fs.lookup_path("/gone").unwrap().ino;
        dev.armed.store(true, Ordering::SeqCst);
        let a = fs.truncate_to_zero(gone);
        eprintln!("truncate under a failing flush: {a:?}");
        let b = fs.create_file(root, b"after", 0o100644);
        eprintln!("create after it: {b:?}");
    }

    let out = kernel_run(&format!(
        r#"
        xfs_logprint -d {image} 2>&1 | head -40
        m=$(mktemp -d)
        if mount -o loop,nouuid {image} "$m"; then
            echo "GONE_SIZE $(stat -c %s "$m/gone")"
            echo "ZERO $([ -e "$m/zero" ] && echo yes || echo no)"
            echo "AFTER $([ -e "$m/after" ] && echo yes || echo no)"
            umount "$m"
            echo MOUNTED
        else
            echo MOUNT_FAILED
        fi
        dmesg | tail -15
        rmdir "$m"
        {repair}
        "#,
        repair = repair::script(&image),
    ));
    eprintln!("{out}");
    panic!("probe");
}
