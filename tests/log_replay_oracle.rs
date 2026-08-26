//! A record this driver writes must be one the Linux kernel replays.
//!
//! Every other test of the log encoder compares our bytes against bytes
//! the kernel wrote. That proves the encoding is faithful, and proves
//! nothing about whether a record placed in a real log is found, trusted
//! and applied — which depends on where it lands, what sequence number
//! it claims, what it says about the record before it, and whether the
//! head-finding scan can see it at all. None of that is visible in a
//! byte comparison.
//!
//! So this writes a transaction and then asks the kernel to act on it.
//!
//! # The shape of the proof
//!
//! The transaction sets a new mode on the root directory, and **the
//! inode on disk is deliberately not touched**. That is what makes the
//! result unambiguous:
//!
//! - if the mode is different after the kernel has had the filesystem,
//!   the only thing that could have changed it is the record;
//! - if the mode is unchanged, the record was ignored — as a torn write,
//!   or because the head scan never found it;
//! - and either way `xfs_repair` then has to find a consistent
//!   filesystem, because a record that is replayed *wrongly* would
//!   produce one that is not.
//!
//! A driver that wrote the inode as well as the log would pass the first
//! check while proving nothing, which is why it does not.
//!
//! # Why this is safe to get wrong
//!
//! A record whose checksum does not verify is treated as a torn write:
//! the kernel truncates the head back and discards it. A malformed
//! record therefore fails this test rather than damaging the fixture,
//! and the fixture is a throwaway copy in any case.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Generate fixtures with
//! `./scripts/vm-build-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// `di_mode` within the inode core, big-endian on disk.
const DI_MODE: usize = 2;

/// The mode to log. Distinctive enough that it cannot be confused with
/// whatever the fixture happens to start with, and still a directory
/// anyone can traverse, so the mount that reads it back does not fail
/// for an unrelated reason.
const NEW_MODE: u16 = 0o0751;

/// Just the permission bits, which is what `stat` reports.
const PERM_BITS: u16 = 0o7777;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Run a script in the oracle VM.
///
/// `None` means the VM could not be reached, which is a reason to skip.
/// It deliberately does **not** cover a script that ran and reported a
/// problem: the scripts below never exit non-zero, so that a kernel
/// refusing the filesystem arrives as output to assert on rather than
/// as an unreachable VM. Conflating the two is how a real failure got
/// reported as a skip the first time this suite ran.
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
    // Every script ends by saying so. Output without it means the guest
    // shell died partway, which is a failure to report, not to skip.
    assert!(
        stdout.contains("DONE"),
        "the VM script did not run to completion:\n{stdout}"
    );
    Some(stdout)
}

/// A working image in the shared folder, removed when it goes out of
/// scope — including on a panic. Every other suite here treats each
/// `.img` in that directory as a fixture to check, so one left behind
/// fails unrelated tests.
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

fn fixture(name: &str) -> Option<PathBuf> {
    let p = share().join(name);
    p.exists().then_some(p)
}

/// The mode byte pair as it sits on the device, read without mounting.
///
/// Mounting is not available once a record has been written: this
/// driver refuses a log with anything outstanding in it, which is the
/// correct behaviour and is asserted for below. So the inode is read at
/// its address, which is a plain device read and cannot be affected by
/// anything the driver believes about the log.
fn mode_at(img: &Path, offset: u64) -> u16 {
    let dev = FileDevice::open(img).expect("open");
    let mut raw = [0u8; 4];
    dev.read_at(offset, &mut raw).expect("read the inode");
    u16::from_be_bytes(raw[DI_MODE..DI_MODE + 2].try_into().expect("2 bytes"))
}

#[test]
fn the_kernel_replays_a_record_this_driver_wrote() {
    let Some(source) = fixture("xfs-default.img") else {
        eprintln!("no xfs-default fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "xfs-log-replay.img");
    let img = scratch.path();

    // Log a core that differs from the one on disk in exactly one field.
    // Everything else is carried across untouched, so a replay cannot
    // corrupt the inode by applying a core this test failed to fill in.
    let (root_at, before) = {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        let at = fs.inode_offset(root).expect("locate the root inode");
        let (_, mut raw) = fs.read_inode_raw(root).expect("read the root inode");

        let before = u16::from_be_bytes(raw[DI_MODE..DI_MODE + 2].try_into().expect("2 bytes"));
        assert_ne!(
            before & PERM_BITS,
            NEW_MODE,
            "the fixture already has the mode this test would set, so it could not tell \
             a replay from nothing happening"
        );

        let mode = (before & !PERM_BITS) | NEW_MODE;
        raw[DI_MODE..DI_MODE + 2].copy_from_slice(&mode.to_be_bytes());

        let lsn = fs
            .log_inode_core(root, &raw)
            .expect("the transaction must be accepted");
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        (at, before)
    };

    // Our own reader has to see the record too. This is not a detour:
    // the head scan that finds it here is the same scan the writer used
    // to place it, so agreement is weak evidence — but *dis*agreement
    // would mean the record was written somewhere nothing looks, and
    // that is worth failing on before asking the kernel.
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

    // The record is the only thing that changed. If this fails, the
    // driver wrote the inode too and the rest of the test proves nothing.
    assert_eq!(
        mode_at(img, root_at),
        before,
        "logging a change must not also apply it — otherwise a replay that never \
         happened is indistinguishable from one that did"
    );

    // Mounting is what triggers recovery; unmounting settles it back to
    // disk. `xfs_repair -n` then has to find nothing wrong: a record
    // applied to the wrong place, or half applied, shows up here even
    // when the mode reads correctly.
    // No `set -e`: a mount the kernel refuses has to reach the
    // assertions below as output, together with what the kernel said
    // about it, rather than killing the script and looking like an
    // unavailable VM.
    let script = r#"
        cp /share/xfs-log-replay.img /tmp/r.img
        dmesg -C >/dev/null 2>&1
        mnt=$(mktemp -d)
        if mount -o loop /tmp/r.img "$mnt"; then
            echo "MODE $(stat -c %a "$mnt")"
            umount "$mnt"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -8
        fi
        rmdir "$mnt" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n /tmp/r.img 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f /tmp/r.img
        echo "DONE"
        "#;
    let Some(out) = vm_run(script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the filesystem after the record was written — the record \
         was found and trusted far enough to try, and then could not be applied:\n{out}"
    );
    let mode = out
        .lines()
        .find_map(|l| l.strip_prefix("MODE "))
        .unwrap_or_else(|| panic!("the VM did not report a mode:\n{out}"))
        .trim();
    let got = u16::from_str_radix(mode, 8).unwrap_or_else(|_| panic!("unreadable mode {mode:?}"));
    assert_eq!(
        got, NEW_MODE,
        "the kernel did not apply the logged core — it read mode {got:o}, and the \
         record asked for {NEW_MODE:o}\n{out}"
    );

    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        report.contains("REPAIR_RC=0"),
        "the checker rejected the filesystem after the replay:\n{report}"
    );
}

/// A read-only mount must refuse to write a record at all.
#[test]
fn a_read_only_mount_refuses_to_log() {
    let Some(source) = fixture("xfs-default.img") else {
        eprintln!("no xfs-default fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "xfs-log-replay-ro.img");
    let img = scratch.path();

    let dev = FileDevice::open(img).expect("open read-only");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
    let root = fs.superblock().rootino;
    let (_, raw) = fs.read_inode_raw(root).expect("read the root inode");

    let err = fs
        .log_inode_core(root, &raw)
        .expect_err("a read-only mount must refuse");
    assert!(matches!(err, fs_xfs::Error::ReadOnly), "got {err}");
}
