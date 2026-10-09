//! A checkpoint whose flush fails is replayed by the kernel to a
//! filesystem `xfs_repair` accepts (#331).
//!
//! A journalled operation writes its log record and then flushes. What a
//! crash leaves is the record on the device and the flush never having
//! returned, and the kernel replays that at its next mount. Nothing else
//! tests that state: the other replay suites let the flush succeed.
//!
//! The device here is a wrapper that lets every write through and then
//! fails the first flush that follows a write into the log, and refuses
//! everything after it, as a machine that has stopped does. The operation
//! is then reported as a failure or not, which this suite does not judge:
//! the record may or may not have been durable, and either way the kernel
//! must be able to mount what is left.
//!
//! For each journalled operation, on a kernel-made image:
//!
//! - the kernel mounts the image (which replays the log),
//! - `xfs_repair -n` calls the result clean, and
//! - the operation is wholly visible or wholly absent, never half of it.
//!
//! The volume is made by `mkfs.xfs` in the harness guest and the kernel
//! and the tool both run there; a guest that cannot be reached is a
//! failure, not a skipped case.

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod common;
use common::{kernel_run, oracle, repair, scratch};

const SUITE: &str = "torn_checkpoint_oracle";

/// The size of the file `mkfs.xfs -p` fills with data.
const BIG: usize = 64 * 1024;

/// What the first write into the empty file carries.
const PAYLOAD_LEN: usize = 6000;

/// A device that fails the flush following a write into the log.
struct TornCheckpoint {
    inner: FileDevice,
    log_start: u64,
    log_end: u64,
    wrote_log: AtomicBool,
    dead: AtomicBool,
    tripped: AtomicBool,
    /// After the failed flush, accept every later write instead of
    /// stopping, as a device with a transient error would (#400).
    keeps_going: bool,
}

impl BlockRead for TornCheckpoint {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

impl BlockDevice for TornCheckpoint {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the device has stopped",
            )));
        }
        self.inner.write_at(offset, buf)?;
        if offset < self.log_end && offset + buf.len() as u64 > self.log_start {
            self.wrote_log.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
    fn flush(&self) -> fs_core::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the device has stopped",
            )));
        }
        if self.wrote_log.swap(false, Ordering::SeqCst) && !self.tripped.load(Ordering::SeqCst) {
            self.dead.store(!self.keeps_going, Ordering::SeqCst);
            self.tripped.store(true, Ordering::SeqCst);
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the flush after the log record failed",
            )));
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }
}

fn payload() -> Vec<u8> {
    (0..PAYLOAD_LEN).map(|i| (i % 251 + 1) as u8).collect()
}

/// A kernel-made image holding `gone` (empty: the driver unlinks only a
/// file with no blocks, and refuses any other before writing a record),
/// `from` and `big` (64 KiB in one extent each) and `empty`, plus the
/// payload file the kernel compares with.
fn base_image(tag: &str) -> scratch::Volume {
    let dir = scratch::dir(SUITE);
    let body: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    for (name, bytes) in [
        ("body", body.as_slice()),
        ("none", &[][..]),
        ("payload", &payload()),
    ] {
        std::fs::write(dir.join(format!("{tag}-{name}")), bytes).unwrap();
    }
    let source = |n: &str| dir.join(format!("{tag}-{n}")).display().to_string();
    std::fs::write(
        dir.join(format!("{tag}-proto")),
        format!(
            "/dev/null\n0 0\nd--755 0 0\ngone ---644 0 0 {n}\nfrom ---644 0 0 {b}\n\
             big ---644 0 0 {b}\nempty ---644 0 0 {n}\n$\n",
            b = source("body"),
            n = source("none"),
        ),
    )
    .unwrap();
    let image = scratch::Volume::empty(SUITE, &format!("{tag}-base.img"), 300 * 1024 * 1024);
    let out = oracle("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(dir.join(format!("{tag}-proto")))
        .arg(image.path())
        .output();
    assert!(out.ok(), "mkfs.xfs: {}{}", out.stdout, out.stderr);
    image
}

/// Run `op` through a device that tears the checkpoint, on a copy of
/// `base`, and return that copy.
fn tear(base: &Path, name: &str, op: impl FnOnce(&Filesystem)) -> scratch::Volume {
    tear_with(base, name, false, op)
}

/// [`tear`], with a device that either stops after the failed flush or
/// keeps accepting writes.
fn tear_with(
    base: &Path,
    name: &str,
    keeps_going: bool,
    op: impl FnOnce(&Filesystem),
) -> scratch::Volume {
    let volume = scratch::Volume::copy_of(SUITE, base, name);
    let (log_start, log_end) = {
        let fs = Filesystem::mount(Arc::new(
            FileDevice::open(volume.path().to_str().unwrap()).unwrap(),
        ))
        .expect("mount read-only");
        let sb = fs.superblock();
        let start = sb.fsblock_offset(sb.logstart);
        (
            start,
            start + u64::from(sb.logblocks) * u64::from(sb.blocksize),
        )
    };
    let dev = Arc::new(TornCheckpoint {
        inner: FileDevice::open_rw(volume.path().to_str().unwrap()).unwrap(),
        log_start,
        log_end,
        wrote_log: AtomicBool::new(false),
        dead: AtomicBool::new(false),
        tripped: AtomicBool::new(false),
        keeps_going,
    });
    {
        let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount rw");
        op(&fs);
    }
    assert!(
        dev.tripped.load(Ordering::SeqCst),
        "{name}: the operation never flushed after writing its record, so nothing was torn"
    );
    volume
}

/// Mount the torn image in the kernel (which replays), report `probe`'s
/// lines, then have `xfs_repair -n` judge it.
fn replay(volume: &scratch::Volume, what: &str, tag: &str, probe: &str) -> String {
    let image = volume.guest();
    let payload_file = scratch::guest_path(&scratch::dir(SUITE).join(format!("{tag}-payload")));
    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        payload={payload_file}
        if mount -o loop,nouuid "$img" "$m"; then
            {probe}
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo "DONE"
        "#
    );
    let out = kernel_run(&script);
    assert!(
        !out.contains("MOUNT_FAILED"),
        "{what}: the kernel refused the volume after a torn checkpoint:\n{out}"
    );
    assert!(
        !out.contains("UMOUNT_FAILED"),
        "{what}: the replayed volume would not unmount:\n{out}"
    );
    repair::assert_agreed(
        &out,
        &format!("{what}, after the kernel replayed a torn checkpoint"),
    );
    out
}

fn has(out: &str, line: &str) -> bool {
    out.lines().any(|l| l.trim() == line)
}

#[test]
fn a_torn_create_is_whole_or_absent() {
    let base = base_image("create");
    let torn = tear(base.path(), "create.img", |fs| {
        let root = fs.superblock().rootino;
        let _ = fs.create_file(root, b"fresh", 0o644);
    });
    let out = replay(
        &torn,
        "create",
        "create",
        r#"if [ -f "$m/fresh" ]; then stat -c 'FRESH %s' "$m/fresh"; else echo ABSENT; fi"#,
    );
    assert!(
        has(&out, "ABSENT") || has(&out, "FRESH 0"),
        "create: neither absent nor an empty file:\n{out}"
    );
}

#[test]
fn a_torn_unlink_is_whole_or_absent() {
    let base = base_image("unlink");
    let torn = tear(base.path(), "unlink.img", |fs| {
        let root = fs.superblock().rootino;
        let _ = fs.unlink_file(root, b"gone");
    });
    let out = replay(
        &torn,
        "unlink",
        "unlink",
        r#"if [ -e "$m/gone" ]; then stat -c 'GONE_PRESENT %s' "$m/gone"; else echo GONE_REMOVED; fi"#,
    );
    assert!(
        has(&out, "GONE_REMOVED") || has(&out, &format!("GONE_PRESENT {BIG}")),
        "unlink: the file is neither removed nor intact:\n{out}"
    );
}

#[test]
fn a_torn_rename_is_whole_or_absent() {
    let base = base_image("rename");
    let torn = tear(base.path(), "rename.img", |fs| {
        let root = fs.superblock().rootino;
        let _ = fs.rename_in_directory(root, b"from", b"to");
    });
    let out = replay(
        &torn,
        "rename",
        "rename",
        r#"[ -e "$m/from" ] && echo FROM_PRESENT || echo FROM_ABSENT
           [ -e "$m/to" ] && echo TO_PRESENT || echo TO_ABSENT"#,
    );
    let before = has(&out, "FROM_PRESENT") && has(&out, "TO_ABSENT");
    let after = has(&out, "FROM_ABSENT") && has(&out, "TO_PRESENT");
    assert!(
        before || after,
        "rename: the name is under both spellings or neither:\n{out}"
    );
}

#[test]
fn a_torn_truncate_to_zero_is_whole_or_absent() {
    let base = base_image("truncate");
    let torn = tear(base.path(), "truncate.img", |fs| {
        let ino = fs.lookup_path("/big").expect("the file").ino;
        let _ = fs.truncate_to_zero(ino);
    });
    let out = replay(
        &torn,
        "truncate",
        "truncate",
        r#"stat -c 'BIG_SIZE %s' "$m/big""#,
    );
    assert!(
        has(&out, "BIG_SIZE 0") || has(&out, &format!("BIG_SIZE {BIG}")),
        "truncate: the file is neither empty nor whole:\n{out}"
    );
}

/// The device recovers after the failed flush, and the mount is asked
/// for more (#400). It must refuse: the next record would start where the
/// failed one did, over whatever part of it reached the device.
#[test]
fn a_mount_whose_log_write_failed_writes_nothing_more() {
    let base = base_image("keeps-going");
    let torn = tear_with(base.path(), "keeps-going.img", true, |fs| {
        let root = fs.superblock().rootino;
        let ino = fs.lookup_path("/big").expect("the file").ino;
        let _ = fs.truncate_to_zero(ino);
        let after = fs.create_file(root, b"after", 0o100644);
        assert!(
            matches!(&after, Err(fs_xfs::Error::Io(m)) if m.contains("log write failed")),
            "a create after the failed log write was not refused: {after:?}"
        );
        let unlink = fs.unlink_file(root, b"gone");
        assert!(
            matches!(&unlink, Err(fs_xfs::Error::Io(m)) if m.contains("log write failed")),
            "an unlink after the failed log write was not refused: {unlink:?}"
        );
    });
    let out = replay(
        &torn,
        "keeps going",
        "keeps-going",
        r#"stat -c 'BIG_SIZE %s' "$m/big"
            [ -e "$m/after" ] && echo AFTER_PRESENT || echo AFTER_ABSENT
            [ -e "$m/gone" ] && echo GONE_PRESENT || echo GONE_ABSENT"#,
    );
    assert!(
        has(&out, "BIG_SIZE 0") || has(&out, &format!("BIG_SIZE {BIG}")),
        "keeps going: the truncated file is neither empty nor whole:\n{out}"
    );
    assert!(
        has(&out, "AFTER_ABSENT") && has(&out, "GONE_PRESENT"),
        "keeps going: a change refused after the failure reached the volume:\n{out}"
    );
}

#[test]
fn a_torn_first_write_is_whole_or_absent() {
    let base = base_image("first-write");
    let torn = tear(base.path(), "first-write.img", |fs| {
        let ino = fs.lookup_path("/empty").expect("the file").ino;
        let _ = fs.write_into_empty_file(ino, &payload());
    });
    let out = replay(
        &torn,
        "first write",
        "first-write",
        r#"stat -c 'EMPTY_SIZE %s' "$m/empty"
           if cmp -s "$m/empty" "$payload"; then echo EMPTY_SAME; fi"#,
    );
    assert!(
        has(&out, "EMPTY_SIZE 0")
            || (has(&out, &format!("EMPTY_SIZE {PAYLOAD_LEN}")) && has(&out, "EMPTY_SAME")),
        "first write: the file is neither empty nor holds exactly what was written:\n{out}"
    );
}
