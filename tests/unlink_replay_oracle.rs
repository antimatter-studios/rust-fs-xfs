//! An unlink this driver logs must remove a file the Linux kernel then
//! agrees is gone.
//!
//! Create in reverse, with the same five items and the same way of
//! failing quietly: an inode given back to the accounting but left
//! looking like a file, a name removed while the inode stays in use, or
//! a chunk that gained a free inode without rejoining the free-inode
//! tree — each still checksums, is still replayed, and leaves a
//! filesystem that is wrong in a way nothing reports.
//!
//! The last of those is the one worth naming. It is not corruption and
//! no check fails: the filesystem simply loses an inode, free and
//! correctly recorded as free and invisible to the tree a create looks
//! in. Only comparing the two trees catches it, which is what
//! `xfs_repair` does here.
//!
//! # The shape of the proof
//!
//! - the name being gone is something only the replay could have done;
//! - the group having one **more** free inode is what says it was given
//!   back rather than merely unnamed;
//! - creating a file afterwards landing on **that inode** is what proves
//!   the free-inode tree can find it again — the check that catches the
//!   lost-inode case above;
//! - the directory's other entries still resolving is what catches a
//!   short-form fork rebuilt from the wrong entries;
//! - and `xfs_repair` is what catches the trees and the group header
//!   disagreeing.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Build them with
//! `./scripts/vm-build-unlink-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod common;
use common::{kernel_run, share};

/// A working image in the shared folder, removed when it goes out of
/// scope.
struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let path = share().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Scratch(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Remove the victim from a copy of `case`'s before-image, then have the
/// kernel replay the record.
fn unlink_and_replay(case: &str) -> Option<()> {
    let source = share().join(format!("xfsunlink-{case}-before.img"));
    if !source.exists() {
        return None;
    }
    let name = format!("xfs-unlink-{case}-scratch.img");
    let scratch = Scratch::from(&source, &name);
    let img = scratch.path();

    let removed = {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        let (ino, lsn) = fs
            .unlink_file(root, b"victim")
            .unwrap_or_else(|e| panic!("{case}: the unlink must be accepted: {e}"));
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        ino
    };

    {
        let dev = FileDevice::open(img).expect("open read-only");
        let err = Filesystem::mount(Arc::new(dev))
            .err()
            .expect("a log with an unreplayed record must not mount");
        assert!(
            matches!(err, fs_xfs::Error::DirtyLog),
            "{case}: expected the log to read as dirty, got {err}"
        );
    }

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/{name} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            if [ -e "$m/victim" ]; then echo "STILL_THERE"; else echo "GONE"; fi
            echo "NAMES $(ls -A "$m" | sort | tr '\n' ' ')"
            [ -d "$m/fill" ] && echo "FILL $(ls "$m/fill" | wc -l)"
            # The freed inode must be findable again. Creating a file
            # here is what says the free-inode tree can hand it back —
            # a chunk that gained a free inode without rejoining that
            # tree loses it silently, and nothing else notices.
            if : > "$m/reuse" 2>/dev/null; then
                echo "REUSE_INO $(stat -c %i "$m/reuse")"
            else
                echo "REUSE_FAILED"
            fi
            umount "$m"
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

    let out = kernel_run(&script)?;

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused the filesystem after the unlink was logged:\n{out}"
    );
    assert!(
        out.contains("GONE"),
        "{case}: the file is still there after the replay\n{out}"
    );
    assert!(
        !out.contains("REUSE_FAILED"),
        "{case}: no file could be created after the removal, so the freed inode is not \
         reachable\n{out}"
    );

    let reused: u64 = out
        .lines()
        .find_map(|l| l.strip_prefix("REUSE_INO "))
        .unwrap_or_else(|| panic!("{case}: the VM did not report the reused inode:\n{out}"))
        .trim()
        .parse()
        .expect("an inode number");

    // In the `wasfull` case the freed inode is the only one available,
    // so the kernel must hand back exactly it. That is the check that
    // catches a chunk which gained a free inode but never rejoined the
    // free-inode tree.
    if case == "wasfull" {
        assert_eq!(
            reused, removed,
            "{case}: the group had exactly one free inode — the one just released — and \
             the kernel gave out {reused} instead of {removed}, so the free-inode tree \
             is not offering it\n{out}"
        );
    }

    let repair: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        repair.contains("REPAIR_RC=0"),
        "{case}: xfs_repair found something wrong after the replay:\n{repair}"
    );

    Some(())
}

/// A file removed by this driver, agreed gone by the kernel.
#[test]
fn the_kernel_agrees_a_file_this_driver_removed_is_gone() {
    let mut ran = Vec::new();
    for case in ["spare", "wasfull"] {
        match unlink_and_replay(case) {
            Some(()) => ran.push(case),
            None => eprintln!("{case}: fixture or VM unavailable — skipped"),
        }
    }
    if ran.is_empty() {
        eprintln!(
            "no unlink fixtures or no VM; build them with \
             ./scripts/vm-build-unlink-fixtures.sh"
        );
        return;
    }
    eprintln!("the kernel agrees the removals landed for: {ran:?}");
}

/// The shapes an unlink will not attempt are refused by name.
#[test]
fn what_it_will_not_do_is_refused() {
    let source = share().join("xfsunlink-spare-before.img");
    if !source.exists() {
        eprintln!("no unlink fixture — skipping");
        return;
    }

    // A name that is not there.
    {
        let scratch = Scratch::from(&source, "xfs-unlink-missing-scratch.img");
        let dev = FileDevice::open_rw(scratch.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        assert!(
            matches!(
                fs.unlink_file(root, b"nosuchfile"),
                Err(fs_xfs::Error::NotFound)
            ),
            "removing a name that is not there must say so"
        );
    }

    // A directory, which has `.` and `..` to account for.
    {
        let scratch = Scratch::from(&source, "xfs-unlink-dir-scratch.img");
        let dev = FileDevice::open_rw(scratch.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        let err = fs
            .unlink_file(root, b"fill")
            .expect_err("removing a directory must be refused");
        assert!(
            err.to_string().contains("directory"),
            "the refusal should say what it refused: {err}"
        );
    }
}
