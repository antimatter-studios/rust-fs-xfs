//! A group's free list is refilled as the kernel refills it, so a tree
//! can keep growing (#197).
//!
//! Every block a group's B+trees grow into comes off its free list (AGFL),
//! and every allocation the kernel makes first tops that list up to
//! `xfs_alloc_min_freelist`, twice the height of each free-space tree and
//! of the reverse map. This driver only took from the list, so on rmapbt,
//! whose leaves split every few dozen extents, a few splits drained it and
//! every later write that needed a tree block was refused.
//!
//! The kernel lays out 500 empty files in 1 KiB-block directories. The
//! driver then writes one block into each, one per mount. Each write adds a
//! reverse-map record of its own, so the tree grows a leaf every 40 writes,
//! and the driver is the one growing it: where the kernel had grown it, the
//! driver's denser layout gave blocks back. The kernel replays every write and `xfs_repair -n` judges it,
//! and no write may be refused, well past the point where the list
//! ran dry. Skips when no kernel is reachable (see
//! `common::transport`); ci-test.sh turns that skip into a failure in CI.

mod common;

use common::{kernel_run, share};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Removes the image however the test ends: every suite reads each `.img`
/// in the share as a fixture. And the share itself when this test made it,
/// because a suite that finds an empty share fails where a missing one
/// skips (`log_oracle` in the fixture-less test jobs).
struct Scratch {
    image: std::path::PathBuf,
    made_share: bool,
}

impl Scratch {
    fn new(image: std::path::PathBuf) -> Self {
        let made_share = !share().exists();
        std::fs::create_dir_all(share()).unwrap();
        Scratch { image, made_share }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.image);
        if self.made_share {
            let _ = std::fs::remove_dir(share());
        }
    }
}

const DIRS: usize = 5;
const PER_DIR: usize = 100;

#[test]
fn writes_on_rmapbt_keep_going_past_the_free_list() {
    let name = format!("agfl-refill-{}.img", std::process::id());
    let image = share().join(&name);
    let _scratch = Scratch::new(image.clone());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(320 * 1024 * 1024))
        .unwrap();

    let Some(built) = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -b size=1024 -d agcount=2 -m rmapbt=1,reflink=0 /share/{name} 2>&1 || echo MKFS_FAILED
        m=$(mktemp -d)
        mount -o loop /share/{name} "$m" || echo MOUNT_FAILED
        for d in $(seq 0 {last_dir}); do
            mkdir "$m/d$d"
            for f in $(seq 0 {last_file}); do
                : > "$m/d$d/f$f"
            done
        done
        umount "$m"
        rmdir "$m"
        echo BUILT
        echo DONE
        "#,
        last_dir = DIRS - 1,
        last_file = PER_DIR - 1,
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        built.contains("BUILT") && !built.contains("FAILED"),
        "building the volume failed:\n{built}"
    );
    let path = image.to_str().unwrap().to_string();

    let replay = format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid /share/{name} "$m"; then
            umount "$m"
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        rmdir "$m"
        out=$(xfs_repair -n /share/{name} 2>&1) && rc=0 || rc=$?
        echo "REPAIR_RC=$rc"
        [ "$rc" = 0 ] || echo "$out" | tail -20
        echo DONE
        "#
    );

    let mut lowest_free_list = u32::MAX;
    let mut written = 0usize;
    for d in 0..DIRS {
        for f in 0..PER_DIR {
            let file = format!("/d{d}/f{f}");
            {
                let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
                let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>)
                    .unwrap_or_else(|e| panic!("mount_rw before {file}: {e:?}"));
                let ino = fs.lookup_path(&file).unwrap().ino;
                fs.write_into_empty_file(ino, &[0x5A; 1024])
                    .unwrap_or_else(|e| panic!("write {file}, after {written} writes: {e:?}"));
            }
            let out = kernel_run(&replay).expect("kernel");
            assert!(
                out.contains("MOUNTED") && out.contains("REPAIR_RC=0"),
                "after writing {file}, the kernel or xfs_repair rejected the volume:\n{out}"
            );
            written += 1;
            let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).unwrap())).unwrap();
            for ag in 0..fs.superblock().agcount {
                lowest_free_list = lowest_free_list.min(fs.read_agf(ag).unwrap().flcount);
            }
        }
    }
    eprintln!("{written} writes; the smallest free list seen held {lowest_free_list} blocks");
}
