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
//!
//! # What the oracle needs from the harness, and did not check (#199)
//!
//! `xfs_repair -n` grades the volume **on disk**. A mounted XFS differs
//! from its own on-disk state in exactly one place -- the summary
//! counters, which are lazy, live in memory while the filesystem is
//! mounted, and are written at unmount -- so a volume graded while still
//! mounted reports `sb_fdblocks N, counted N-1` for every block the
//! replay has just claimed. That line reads as a defect in this driver
//! and is not one.
//!
//! The first version ran `umount` and announced MOUNTED whatever it
//! returned, and `kernel_run` keeps stdout, so the error went nowhere.
//! Now the unmount is checked, retried while the mount is busy, and the
//! mount point is confirmed gone before `xfs_repair` is asked anything.
//!
//! And when something does fail, the failure is made readable rather
//! than left to the next reader to reproduce: the group headers and the
//! superblock counters from either side of the mount (headers that
//! disagree with the trees are this driver's; a superblock that did not
//! move while the headers did is the mount's), the whole of
//! `xfs_repair`'s output rather than its last twenty lines, the kernel
//! ring buffer, and the volume itself, which is kept for the job to
//! upload instead of being deleted on the way out.

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
        // KEPT WHEN THE TEST FAILED, because this step keeps no artefact
        // of its own and the next occurrence is as unreadable as the
        // last one without the volume it happened to (#199). A failure
        // ends the job, so nothing after this point reads the share and
        // a kept image costs nothing but the runner's disk.
        if std::thread::panicking() {
            eprintln!(
                "keeping {} for the artefact upload: the volume this failed on",
                self.image.display()
            );
            return;
        }
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
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m" 2>/dev/null
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

    // WHAT THE FAILURE NEEDS TO BE READABLE. The first occurrence of
    // this failing (#199, run 35268094013) reported
    // `sb_fdblocks 261615, counted 261614` and twenty lines of
    // `xfs_repair` after it, which cannot tell apart the two things
    // that produce exactly that:
    //
    //   * the group headers this driver wrote disagree with the trees
    //     it laid out -- a defect here, visible as the `agf_*` lines
    //     `tail -20` cut off; or
    //   * the headers are right and the kernel's summary counters were
    //     not brought up to date over the mount -- visible only by
    //     comparing the counters before the mount with the ones after
    //     it.
    //
    // So both sets of counters are taken, kept in variables, and
    // printed only when something fails, and `xfs_repair` is quoted in
    // full rather than from the tail. A mount that cannot be undone is
    // reported rather than swallowed: `umount` failing left the volume
    // mounted while `xfs_repair` read it, and a mounted XFS differs
    // from its own on-disk state in exactly the summary counters.
    let replay = format!(
        r#"
        counters() {{
            xfs_db -r -c 'sb 0' -c 'print fdblocks icount ifree' /share/{name} 2>&1
            for ag in 0 1; do
                echo "== agf $ag"
                xfs_db -r -c "agf $ag" \
                    -c 'print freeblks flcount flfirst fllast btreeblks rmapblocks levels longest' \
                    /share/{name} 2>&1
            done
        }}
        before=$(counters)
        m=$(mktemp -d)
        mounted=no
        how=
        # THE STDERR OF THE MOUNT IS EVIDENCE, NOT NOISE. `mount` falls
        # back to a read-only mount when it cannot get a writable one,
        # and says so only on stderr -- which `kernel_run` does not keep.
        # A read-only XFS recovers the log (writing the group headers)
        # and then skips the superblock counters at unmount, which is the
        # same `sb_fdblocks N, counted N-1` an unnoticed failed unmount
        # produces. `findmnt` after the fact says which it was.
        if mount_said=$(mount -o loop,nouuid /share/{name} "$m" 2>&1); then
            how=$(findmnt -n -o SOURCE,FSTYPE,OPTIONS "$m" 2>&1)
            # A read-only mount is not this oracle's mount. XFS recovers
            # the log even read-only -- it has to -- so the group headers
            # reach the disk, and then the superblock counters do not,
            # because a read-only filesystem writes nothing at unmount.
            # Graded, that is indistinguishable from a driver that
            # miscounted, so it is named here instead.
            if findmnt -n -o OPTIONS "$m" 2>/dev/null | grep -qw ro; then
                echo "MOUNTED_READ_ONLY: $how"
            fi
            # UNMOUNTING IS PART OF THE ORACLE, NOT ITS CLEANUP.
            # `xfs_repair` grades what is on disk, and a live XFS differs
            # from its own on-disk state in exactly one place: the
            # summary counters, which are written at unmount and nowhere
            # else. So an unmount that failed and went unremarked is
            # graded as `sb_fdblocks N, counted N-1` -- a line that reads
            # as a defect in this driver and is not one. The old script
            # ran `umount` and announced MOUNTED whatever it returned,
            # and its stderr went nowhere (`kernel_run` keeps stdout).
            #
            # A busy mount is ordinary on a shared runner -- something
            # else opens the loop device for a moment -- so it is
            # retried. What must not happen is grading a filesystem that
            # is still mounted, so the flag is set by a umount that
            # succeeded and by nothing else.
            i=0
            while [ $i -lt 10 ]; do
                err=$(umount "$m" 2>&1) && {{ mounted=yes; break; }}
                i=$((i+1))
                sleep 0.2
            done
            if [ "$mounted" = yes ]; then
                echo MOUNTED
            else
                echo "UMOUNT_FAILED after $i attempts: $err"
                umount -l "$m" 2>&1
            fi
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        if mountpoint -q "$m" 2>/dev/null; then
            echo STILL_MOUNTED
        fi
        rmdir "$m" 2>/dev/null
        after=$(counters)
        out=$(xfs_repair -n /share/{name} 2>&1) && rc=0 || rc=$?
        echo "REPAIR_RC=$rc"
        if [ "$rc" != 0 ] || [ "$mounted" != yes ]; then
            echo "== xfs_repair -n, in full"
            echo "$out"
            echo "== counters before the mount"
            echo "$before"
            echo "== counters after the mount"
            echo "$after"
            echo "== how the kernel had it mounted"
            echo "$how"
            echo "== what mount itself said"
            echo "$mount_said"
            echo "== what the kernel said while it had this volume"
            dmesg | tail -30
            xfs_logprint -t /share/{name} 2>&1 | tail -20
        fi
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
                out.contains("MOUNTED")
                    && out.contains("REPAIR_RC=0")
                    && !out.contains("STILL_MOUNTED")
                    && !out.contains("MOUNTED_READ_ONLY"),
                "after writing {file} (write {written}), the kernel or xfs_repair \
                 rejected the volume. The counters on either side of the mount say \
                 which: headers that disagree with the trees are this driver's, and \
                 a superblock that did not move while the group headers did is the \
                 mount's.\n{out}"
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
