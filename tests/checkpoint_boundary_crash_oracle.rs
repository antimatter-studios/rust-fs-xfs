//! A crash at any checkpoint of a mount's sequence replays to a prefix of
//! that sequence (#365).
//!
//! One mount runs several journalled operations, each built on the state
//! the ones before it logged (#89). A crash can land at any flush in
//! that sequence, and the kernel replays whatever reached the log. What
//! it replays to has to be a prefix of the sequence: every operation that
//! returned before the crash is there, the one that was running is wholly
//! there or wholly not, and nothing after it is.
//!
//! The device here counts flushes and dies at the n-th: the write before
//! it lands, the flush fails, and every call after it fails, as a machine
//! that stopped would. The suite runs the sequence once for each n, from
//! the first flush until a run in which the sequence finishes before the
//! device dies, so every checkpoint boundary, and every record boundary
//! inside a checkpoint split across records, is a crash point once.
//!
//! The volume is made by `mkfs.xfs -p` in the harness guest. After each
//! crash the kernel mounts a copy, which replays the log, reports what it
//! holds, and `xfs_repair -n` judges it.

mod common;

use common::{kernel_run, oracle, repair, scratch};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

const SUITE: &str = "checkpoint_boundary_crash_oracle";

/// The size of `big`, which the last operation truncates.
const BIG: usize = 64 * 1024;

/// What the write into `empty` carries.
fn payload() -> Vec<u8> {
    (0..6000).map(|i| (i % 251 + 1) as u8).collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// A device that dies at its n-th flush: the writes before it land, the
/// flush fails, and everything after it fails.
struct DiesAtFlush {
    inner: FileDevice,
    flushes: AtomicU64,
    die_at: u64,
    dead: AtomicBool,
}

impl BlockRead for DiesAtFlush {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

impl BlockDevice for DiesAtFlush {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the device has stopped",
            )));
        }
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> fs_core::Result<()> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the device has stopped",
            )));
        }
        if self.flushes.fetch_add(1, Ordering::SeqCst) + 1 == self.die_at {
            self.dead.store(true, Ordering::SeqCst);
            return Err(fs_core::Error::Io(std::io::Error::other(
                "the machine stopped",
            )));
        }
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        self.inner.is_writable()
    }
}

/// What the volume holds after the first `done` operations of
/// [`run_sequence`], in the words the kernel probe prints.
fn expected(done: usize) -> Vec<String> {
    let empty = if done >= 3 {
        sha256_hex(&payload())
    } else {
        sha256_hex(&[])
    };
    vec![
        format!(
            "A {}",
            if (1..4).contains(&done) {
                "PRESENT"
            } else {
                "ABSENT"
            }
        ),
        format!("B {}", if done == 4 { "PRESENT" } else { "ABSENT" }),
        format!("D {}", if done >= 2 { "PRESENT" } else { "ABSENT" }),
        format!("EMPTY_SHA {empty}"),
        format!("BIG_SIZE {}", if done >= 6 { 0 } else { BIG }),
    ]
}

/// The kernel's probe, printing the lines [`expected`] describes.
const PROBE: &str = r#"
    [ -e "$m/a" ] && echo "A PRESENT" || echo "A ABSENT"
    [ -e "$m/b" ] && echo "B PRESENT" || echo "B ABSENT"
    [ -d "$m/d" ] && echo "D PRESENT" || echo "D ABSENT"
    echo "EMPTY_SHA $(sha256sum < "$m/empty" | cut -d' ' -f1)"
    echo "BIG_SIZE $(stat -c %s "$m/big")"
"#;

/// The sequence, on one mount: create, mkdir, first write, rename,
/// unlink, truncate. Returns how many operations returned `Ok` before
/// one failed, or all six.
fn run_sequence(fs: &Filesystem) -> usize {
    let root = fs.superblock().rootino;
    let empty = fs.lookup_path("/empty").expect("empty").ino;
    let big = fs.lookup_path("/big").expect("big").ino;
    let steps: [&dyn Fn() -> fs_xfs::Result<()>; 6] = [
        &|| fs.create_file(root, b"a", 0o100644).map(drop),
        &|| fs.create_directory(root, b"d", 0o040755).map(drop),
        &|| fs.write_into_empty_file(empty, &payload()).map(drop),
        &|| fs.rename_in_directory(root, b"a", b"b").map(drop),
        &|| fs.unlink_file(root, b"b").map(drop),
        &|| fs.truncate_to_zero(big).map(drop),
    ];
    for (done, step) in steps.iter().enumerate() {
        if step().is_err() {
            return done;
        }
    }
    steps.len()
}

/// A kernel-made image holding `empty` and `big`.
fn base_image() -> scratch::Volume {
    let dir = scratch::dir(SUITE);
    let body: Vec<u8> = (0..BIG).map(|i| (i % 251) as u8).collect();
    let none = dir.join("none");
    let full = dir.join("body");
    std::fs::write(&none, b"").unwrap();
    std::fs::write(&full, &body).unwrap();
    let proto = dir.join("proto");
    std::fs::write(
        &proto,
        format!(
            "/dev/null\n0 0\nd--755 0 0\nempty ---644 0 0 {}\nbig ---644 0 0 {}\n$\n",
            none.display(),
            full.display()
        ),
    )
    .unwrap();
    let image = scratch::Volume::empty(SUITE, "base.img", 300 * 1024 * 1024);
    let out = oracle("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(&proto)
        .arg(image.path())
        .output();
    assert!(out.ok(), "mkfs.xfs: {}{}", out.stdout, out.stderr);
    image
}

/// Run the sequence on a copy of `base` whose device dies at flush `n`.
/// Returns the copy, how many operations completed, and whether the
/// device died at all.
fn crash_at(base: &Path, n: u64) -> (scratch::Volume, usize, bool) {
    let volume = scratch::Volume::copy_of(SUITE, base, &format!("crash-{n}.img"));
    let dev = Arc::new(DiesAtFlush {
        inner: FileDevice::open_rw(volume.path().to_str().unwrap()).unwrap(),
        flushes: AtomicU64::new(0),
        die_at: n,
        dead: AtomicBool::new(false),
    });
    let done = {
        let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount rw");
        run_sequence(&fs)
    };
    (volume, done, dev.dead.load(Ordering::SeqCst))
}

/// What the kernel finds after replaying `volume`, checked by
/// `xfs_repair -n`.
fn replayed(volume: &scratch::Volume, what: &str) -> Vec<String> {
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {PROBE}
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo DONE
        "#
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "{what}: the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, what);
    let keys = ["A ", "B ", "D ", "EMPTY_SHA ", "BIG_SIZE "];
    keys.iter()
        .map(|k| {
            out.lines()
                .map(str::trim)
                .find(|l| l.starts_with(k))
                .unwrap_or_else(|| panic!("{what}: the probe printed no `{k}`:\n{out}"))
                .to_string()
        })
        .collect()
}

#[test]
fn a_crash_at_every_flush_replays_to_a_prefix_of_the_sequence() {
    let base = base_image();
    let mut crashes = 0;
    for n in 1.. {
        let (volume, done, died) = crash_at(base.path(), n);
        if !died {
            // The sequence finished before flush n: every boundary has
            // been a crash point, and the whole sequence must be there.
            let found = replayed(&volume, "the uncrashed sequence");
            assert_eq!(found, expected(6), "the whole sequence did not replay");
            break;
        }
        crashes += 1;
        let what = format!("a crash at flush {n}, after {done} operations");
        let found = replayed(&volume, &what);
        assert!(
            found == expected(done) || found == expected(done + 1),
            "{what}: the replayed volume is not a prefix of the sequence.\n\
             found:    {found:?}\nexpected: {:?}\n      or: {:?}",
            expected(done),
            expected(done + 1)
        );
        assert!(
            n < 64,
            "the sequence flushed more than 64 times; something loops"
        );
    }
    assert!(
        crashes >= 6,
        "only {crashes} crash points for six journalled operations: the device is not \
         seeing each checkpoint's flush"
    );
}
