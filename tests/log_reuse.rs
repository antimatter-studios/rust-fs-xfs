//! A mount keeps going past the end of the log, in bounded memory (#89).
//!
//! Every record a mount writes is kept in memory until something writes the
//! metadata it describes where it belongs — the push an XFS mount makes
//! through the AIL. Without that push two things go wrong: the log fills
//! and the mount stops, and the memory grows for as long as the mount runs.
//! Measured before this: 32,751 operations, then "the checkpoint needs 4
//! basic blocks and only 2 remain before the log wraps".
//!
//! Here one mount writes far more than the log holds, so the ring is reused
//! several times over. The kernel then replays what is left and must arrive
//! at exactly the files the sequence describes, with `xfs_repair` clean —
//! and the memory held must stay under the bound throughout.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "log_reuse";
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Directories the kernel makes for this to work in. Four is enough: each
/// holds one name at a time, because every create is undone by an unlink.
const DIRS: u64 = 4;

/// The ring has to be started again at least this many times, which is what
/// says the run reused the log rather than fitting inside it.
const WRAPS_WANTED: usize = 1;

/// A stop, so a run that is not wrapping fails rather than going forever.
const MOST_OPERATIONS: u32 = 120_000;

/// What this driver holds in memory before it pushes, from `fs.rs`.
const MAX_DIRTY_BYTES: usize = 16 * 1024 * 1024;

#[test]
fn a_mount_writes_past_the_end_of_the_log() {
    // NO FIXTURE DIRECTORY MEANS NO FIXTURE SET. This builds its own
    // volume, but it builds it in the share, and a share that exists is
    // what the suites scanning it take for a fixture set: creating one
    // here makes them fail where they would have skipped. The job that
    // runs this builds the fixtures first, so the directory is there.
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        320 * 1024 * 1024,
    );
    let image = scratch.path().to_path_buf();
    let name = scratch.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        for d in $(seq 0 {last}); do mkdir "$m/d$d"; done
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#,
        last = DIRS - 1
    ));
    let Some(built) = built else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let path = image.to_str().unwrap().to_string();

    let mut most_held = 0usize;
    let mut operations = 0u32;
    {
        let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        // Create and unlink in pairs, so the directories stay short-form and
        // the only thing that accumulates is records. It runs until the ring
        // has been started again often enough to say so, rather than for a
        // count that would have to be re-guessed for every log size.
        for step in 0.. {
            if fs.log_wraps() >= WRAPS_WANTED {
                break;
            }
            assert!(
                operations < MOST_OPERATIONS,
                "{operations} operations and the log has been reused {} times: it is not \
                 wrapping",
                fs.log_wraps()
            );
            // A create and an unlink of the same name: the directory stays
            // short-form, nothing accumulates but records, and the pair is
            // the cheapest way to move the log on.
            let dir = fs
                .lookup_path(&format!("/d{}", step % DIRS))
                .expect("a directory the kernel made")
                .ino;
            let file = format!("f{step:06}");
            fs.create_file(dir, file.as_bytes(), 0o100644)
                .unwrap_or_else(|e| panic!("create at step {step}: {e:?}"));
            fs.unlink_file(dir, file.as_bytes())
                .unwrap_or_else(|e| panic!("unlink at step {step}: {e:?}"));
            operations += 2;
            most_held = most_held.max(fs.dirty_bytes());
        }
        eprintln!(
            "{operations} operations reused the log {} times, holding at most {most_held} \
             bytes",
            fs.log_wraps()
        );
        // Something to find afterwards, left behind by the last few
        // operations rather than by the first.
        for k in 0..4u32 {
            let dir = fs
                .lookup_path(&format!("/d{k}"))
                .expect("a kept directory")
                .ino;
            fs.create_file(dir, format!("kept{k}").as_bytes(), 0o100644)
                .expect("the file this mount leaves behind");
        }
        // And the push a caller makes when it is finished.
        fs.sync().expect("sync");
        assert_eq!(fs.dirty_bytes(), 0, "a sync leaves nothing held");
    }

    assert!(
        most_held <= MAX_DIRTY_BYTES,
        "the mount held {most_held} bytes of logged metadata, past the {MAX_DIRTY_BYTES} \
         it pushes at"
    );

    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {name} "$m"; then
            echo "KEPT $(find "$m" -name 'kept*' | wc -l)"
            echo "LEFTOVER $(find "$m" -name 'f0*' | wc -l)"
            umount "$m" || echo UMOUNT_FAILED
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | grep -i xfs | tail -25
        fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ))
    .expect("kernel");

    assert!(
        out.contains("MOUNTED"),
        "after {operations} operations the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, &format!("after {operations} operations"));
    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(field("KEPT"), "4", "the files the mount left behind");
    assert_eq!(
        field("LEFTOVER"),
        "0",
        "every created-then-unlinked name is gone, including the ones whose records the \
         log has since overwritten"
    );
}
