//! A rename this driver logs must be one the Linux kernel carries out.
//!
//! This is the first change to a *directory* that goes through the log,
//! and the first transaction with two items. Everything about it that
//! could be wrong is wrong quietly: a record with a mis-sized fork, a
//! reused directory cookie or a stale entry count still checksums, is
//! still found, and is still replayed — into a directory that then reads
//! back differently than intended, or not at all.
//!
//! # The shape of the proof
//!
//! The inodes on disk are deliberately not touched. Only the record is
//! written, so:
//!
//! - the old name disappearing and the new one appearing is something
//!   only the replay could have done;
//! - the renamed file keeping its **inode number** is what separates a
//!   rename from a delete and a create;
//! - the untouched sibling still being there is what catches a fork
//!   rebuilt from the wrong entries;
//! - and `xfs_repair` afterwards is what catches a directory that reads
//!   correctly and is structurally wrong anyway.
//!
//! Fixtures are gitignored and generated. Build them with
//! `chore fixtures -- log`; the image is built on every run, so one that
//! is not there fails rather than quietly leaving the rename unjudged.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::Path;
use std::sync::Arc;

mod common;
use common::{fixture, kernel_run, repair, scratch};

/// Where this suite's scratch volumes live: under
/// `.vm-share/scratch/`, not beside the fixtures another suite is
/// reading while this one writes (#223).
const SUITE: &str = "rename_oracle";

/// A directory small enough to live inside its inode, built by the
/// fixture script with two equal-length names.
const DIR: &str = "/sf";

/// The inode a name resolves to, and the directory's own inode.
fn inodes_of(img: &Path, name: &str) -> (u64, u64) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
    let dir = fs.lookup_path(DIR).expect("find the directory");
    let entry = fs
        .lookup_path(&format!("{DIR}/{name}"))
        .unwrap_or_else(|e| panic!("{DIR}/{name}: {e}"));
    (dir.ino, entry.ino)
}

#[test]
fn the_kernel_carries_out_a_rename_this_driver_logged() {
    let source = fixture("xfslog-b4096-i512.img");
    let scratch = scratch::Volume::copy_of(SUITE, &source, "xfs-rename.img");
    let img = scratch.path();

    let (dir_ino, moved_ino) = inodes_of(img, "aaaa");

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let lsn = fs
            .rename_in_directory(dir_ino, b"aaaa", b"cccc")
            .expect("the rename must be accepted");
        assert_ne!(lsn, 0, "a record must be given a sequence number");
    }

    // NOTHING BUT THE LOG HAS CHANGED, and a read-only mount replays it
    // in memory (#90): the new name is there and the old one is not,
    // before the kernel has seen any of it.
    {
        let dev = FileDevice::open(img).expect("open read-only");
        let fs = Filesystem::mount(Arc::new(dev))
            .expect("a volume whose log holds a record mounts, replaying it");
        let (dir, raw) = fs.read_inode_raw(dir_ino).expect("the directory");
        let names: Vec<String> = fs
            .read_dir(&dir, &raw)
            .expect("the directory's entries")
            .iter()
            .map(|e| String::from_utf8_lossy(&e.name).to_string())
            .collect();
        assert!(
            names.iter().any(|n| n == "cccc") && !names.iter().any(|n| n == "aaaa"),
            "replaying the rename in memory left {names:?}"
        );
    }

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {source} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "NAMES $(ls "$m/sf" | sort | tr '\n' ' ')"
            echo "INO $(stat -c %i "$m/sf/cccc" 2>/dev/null || echo none)"
            # RETRIED ONCE. A busy unmount under a loaded runner is
            # ordinary and clears in a moment; one that does not is the
            # failure worth reporting, because the kernel writes the
            # summary counters at unmount and nothing else does.
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        else
            echo "MOUNT_FAILED"
            dmesg | tail -8
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo "DONE"
        echo DONE
        "#,
        source = scratch.guest(),
    );
    // The replay always happens in the harness guest, so the rename is
    // always put to the kernel.
    let out = kernel_run(&script);

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the filesystem after the rename was logged:\n{out}"
    );
    assert!(
        !out.contains("UMOUNT_FAILED"),
        "the volume could not be unmounted, so the summary counters were never \
         written back to it. `xfs_repair` reports `sb_fdblocks N, counted N-1` for \
         exactly that -- the free-block count it disagrees about is the one the \
         unmount never wrote, not one this driver got wrong:\n{out}"
    );

    let names = out
        .lines()
        .find_map(|l| l.strip_prefix("NAMES "))
        .unwrap_or_else(|| panic!("the VM did not list the directory:\n{out}"))
        .trim();
    assert_eq!(
        names, "bbbb cccc",
        "the directory should hold the renamed entry and its untouched sibling\n{out}"
    );

    // A delete followed by a create would satisfy the listing and not
    // this: a rename keeps the file it names.
    let ino = out
        .lines()
        .find_map(|l| l.strip_prefix("INO "))
        .unwrap_or_else(|| panic!("the VM did not report an inode:\n{out}"))
        .trim();
    assert_eq!(
        ino,
        moved_ino.to_string(),
        "the new name should resolve to the inode the old name did\n{out}"
    );

    repair::assert_agreed(&out, "the filesystem after the rename");
}

/// Renaming onto a name that is taken must be refused, and must leave
/// the log alone — a partially built record would be replayed.
#[test]
fn a_name_that_is_taken_is_refused() {
    let source = fixture("xfslog-b4096-i512.img");
    let scratch = scratch::Volume::copy_of(SUITE, &source, "xfs-rename-taken.img");
    let img = scratch.path();
    let (dir_ino, _) = inodes_of(img, "aaaa");

    let dev = FileDevice::open_rw(img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let err = fs
        .rename_in_directory(dir_ino, b"aaaa", b"bbbb")
        .expect_err("renaming onto an existing name must be refused");
    assert!(matches!(err, fs_xfs::Error::AlreadyExists), "got {err}");
    drop(fs);

    // Refusing has to mean nothing was written. If a record went in
    // anyway, this mount reports the log as dirty.
    let dev = FileDevice::open(img).expect("open read-only");
    Filesystem::mount(Arc::new(dev)).expect("a refused rename must leave the log clean");
}

/// A name that is not there is not found, rather than being invented.
#[test]
fn a_name_that_is_not_there_is_refused() {
    let source = fixture("xfslog-b4096-i512.img");
    let scratch = scratch::Volume::copy_of(SUITE, &source, "xfs-rename-missing.img");
    let img = scratch.path();
    let (dir_ino, _) = inodes_of(img, "aaaa");

    let dev = FileDevice::open_rw(img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    let err = fs
        .rename_in_directory(dir_ino, b"nothing", b"cccc")
        .expect_err("renaming a name that is not there must be refused");
    assert!(matches!(err, fs_xfs::Error::NotFound), "got {err}");
}

/// A directory past short form is refused by name, not attempted — it
/// lives in a block, and rewriting one logs a buffer item this cannot
/// yet produce.
#[test]
fn a_directory_past_short_form_is_refused() {
    let source = fixture("xfslog-b4096-i512.img");
    let scratch = scratch::Volume::copy_of(SUITE, &source, "xfs-rename-big.img");
    let img = scratch.path();

    let dev = FileDevice::open_rw(img).expect("open read-write");
    let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
    // `/logged` holds 200 entries, far past what an inode carries.
    let big = fs.lookup_path("/logged").expect("find the directory");
    let err = fs
        .rename_in_directory(big.ino, b"f1", b"f9999")
        .expect_err("a directory outside the inode must be refused");
    assert!(
        format!("{err}").contains("outgrown the inode"),
        "the refusal should say why: {err}"
    );
}

/// A read-only mount refuses before reading anything.
#[test]
fn a_read_only_mount_refuses_to_rename() {
    let source = fixture("xfslog-b4096-i512.img");
    let scratch = scratch::Volume::copy_of(SUITE, &source, "xfs-rename-ro.img");
    let img = scratch.path();
    let (dir_ino, _) = inodes_of(img, "aaaa");

    let dev = FileDevice::open(img).expect("open read-only");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
    let err = fs
        .rename_in_directory(dir_ino, b"aaaa", b"cccc")
        .expect_err("a read-only mount must refuse");
    assert!(matches!(err, fs_xfs::Error::ReadOnly), "got {err}");
}
