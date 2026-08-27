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
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Build them with
//! `./scripts/vm-build-log-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// A directory small enough to live inside its inode, built by the
/// fixture script with two equal-length names.
const DIR: &str = "/sf";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Run a script in the oracle VM.
///
/// `None` means the VM could not be reached. It deliberately does not
/// cover a script that ran and reported a problem — the scripts below
/// never exit non-zero, so a kernel refusing the filesystem arrives as
/// output to assert on rather than as an unreachable VM.
fn vm_run(script: &str) -> Option<String> {
    let out = Command::new(repo().join("scripts/vm.sh"))
        .arg("run")
        .arg(script)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        eprintln!(
            "vm.sh run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    assert!(
        stdout.contains("DONE"),
        "the VM script did not run to completion:\n{stdout}"
    );
    Some(stdout)
}

/// A working image in the shared folder, removed when it goes out of
/// scope. Every other suite treats each `.img` there as a fixture, so
/// one left behind fails unrelated tests.
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

fn fixture() -> Option<PathBuf> {
    let p = share().join("xfslog-b4096-i512.img");
    p.exists().then_some(p)
}

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
    let Some(source) = fixture() else {
        eprintln!("no xfslog fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "xfs-rename.img");
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

    // Nothing but the log has changed. Our own reader still sees the old
    // name, and refuses the volume because the log has work in it.
    {
        let dev = FileDevice::open(img).expect("open read-only");
        let err = Filesystem::mount(Arc::new(dev))
            .err()
            .expect("a log with an unreplayed record must not mount");
        assert!(
            matches!(err, fs_xfs::Error::DirtyLog),
            "expected the log to read as dirty, got {err}"
        );
    }

    let script = r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/xfs-rename.img "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "NAMES $(ls "$m/sf" | sort | tr '\n' ' ')"
            echo "INO $(stat -c %i "$m/sf/cccc" 2>/dev/null || echo none)"
            umount "$m"
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
        "#;
    let Some(out) = vm_run(script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the filesystem after the rename was logged:\n{out}"
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

    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        report.contains("REPAIR_RC=0"),
        "the checker rejected the filesystem after the rename:\n{report}"
    );
}

/// Renaming onto a name that is taken must be refused, and must leave
/// the log alone — a partially built record would be replayed.
#[test]
fn a_name_that_is_taken_is_refused() {
    let Some(source) = fixture() else {
        return;
    };
    let scratch = Scratch::from(&source, "xfs-rename-taken.img");
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
    let Some(source) = fixture() else {
        return;
    };
    let scratch = Scratch::from(&source, "xfs-rename-missing.img");
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
    let Some(source) = fixture() else {
        return;
    };
    let scratch = Scratch::from(&source, "xfs-rename-big.img");
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
    let Some(source) = fixture() else {
        return;
    };
    let scratch = Scratch::from(&source, "xfs-rename-ro.img");
    let img = scratch.path();
    let (dir_ino, _) = inodes_of(img, "aaaa");

    let dev = FileDevice::open(img).expect("open read-only");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
    let err = fs
        .rename_in_directory(dir_ino, b"aaaa", b"cccc")
        .expect_err("a read-only mount must refuse");
    assert!(matches!(err, fs_xfs::Error::ReadOnly), "got {err}");
}
