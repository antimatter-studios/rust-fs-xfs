//! A truncate this driver logs must be one the Linux kernel carries out.
//!
//! This is the first transaction that changes something other than an
//! inode, and the first with buffer items in it. Everything about it
//! that could be wrong is wrong quietly: a record whose dirty-chunk
//! bitmap does not match its data operations, whose block addresses are
//! in the wrong unit, or whose free-space tree is internally consistent
//! but disagrees with the group header, still checksums, is still found,
//! and is still replayed — onto a filesystem whose free space is then
//! wrong in a way nothing reports until two files are given the same
//! blocks.
//!
//! # The shape of the proof
//!
//! Nothing on disk is touched. Only the record is written, so:
//!
//! - the file becoming empty is something only the replay could have
//!   done;
//! - the blocks reappearing in the group's free space is what separates
//!   a truncate from an inode that merely says zero;
//! - the untouched neighbours still reading correctly is what catches
//!   free space handed out twice;
//! - and `xfs_repair` afterwards is what catches trees that are
//!   plausible on their own and inconsistent with each other.
//!
//! The last matters most here. A free-space tree can be well-formed,
//! ordered, and wrong — and the check that catches it is the one that
//! compares the two trees and the group header against each other.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Build them with
//! `./scripts/vm-build-truncate-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// The file the fixtures truncate.
const VICTIM: &str = "/victim";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Run a script in the oracle VM. `None` means the VM was unreachable.
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

/// Log a truncate of the victim into a copy of `case`'s before-image,
/// then have the kernel replay it.
///
/// The case names which of the four merge outcomes the fixture was built
/// to produce, so a failure says which arrangement of free space broke
/// rather than only that something did.
fn replay_case(case: &str) -> Option<()> {
    let source = share().join(format!("xfstrunc-{case}-before.img"));
    if !source.exists() {
        return None;
    }
    let name = format!("xfs-trunc-{case}-scratch.img");
    let scratch = Scratch::from(&source, &name);
    let img = scratch.path();

    let victim = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
        fs.lookup_path(VICTIM).expect("the victim is present").ino
    };

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let lsn = fs
            .truncate_to_zero(victim)
            .unwrap_or_else(|e| panic!("{case}: the truncate must be accepted: {e}"));
        assert_ne!(lsn, 0, "a record must be given a sequence number");
    }

    // Nothing but the log has changed, so our own reader refuses the
    // volume rather than reading past an unapplied record.
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
            echo "SIZE $(stat -c %s "$m/victim")"
            echo "BLOCKS $(stat -c %b "$m/victim")"
            # The neighbours must still read back whole. Free space
            # handed out twice shows up here before it shows up
            # anywhere else.
            ok=yes
            for f in f1 f2 f4 f5; do
                [ -e "$m/$f" ] || continue
                cksum "$m/$f" >/dev/null 2>&1 || ok=no
                [ "$(stat -c %s "$m/$f")" = "1048576" ] || ok=no
            done
            echo "NEIGHBOURS $ok"
            echo "DF $(df --output=avail -k "$m" | tail -1)"
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

    let out = vm_run(&script)?;

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused the filesystem after the truncate was logged:\n{out}"
    );

    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("{case}: the VM did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };

    assert_eq!(
        field("SIZE"),
        "0",
        "{case}: the file should be empty after the replay\n{out}"
    );
    assert_eq!(
        field("BLOCKS"),
        "0",
        "{case}: the file should hold no blocks after the replay\n{out}"
    );
    assert_eq!(
        field("NEIGHBOURS"),
        "yes",
        "{case}: a neighbouring file no longer reads back correctly, which is what \
         free space handed out twice looks like\n{out}"
    );

    // A repair that finds nothing is what says the two trees and the
    // group header agree. Trees can be well-formed and still wrong.
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

/// Every arrangement of neighbouring free space, replayed by the kernel.
#[test]
fn the_kernel_carries_out_a_truncate_this_driver_logged() {
    let mut ran = Vec::new();
    for case in ["lone", "after", "before", "between"] {
        match replay_case(case) {
            Some(()) => ran.push(case),
            None => eprintln!("{case}: fixture or VM unavailable — skipped"),
        }
    }
    if ran.is_empty() {
        eprintln!(
            "no truncate fixtures or no VM; build them with \
             ./scripts/vm-build-truncate-fixtures.sh"
        );
        return;
    }
    eprintln!("the kernel replayed the truncate for: {ran:?}");
}
