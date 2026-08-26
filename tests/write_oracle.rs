//! An in-place write must leave a filesystem the reference tooling
//! still calls valid, holding exactly the bytes we asked for.
//!
//! Both halves are load-bearing and neither is sufficient alone.
//!
//! Reading our own write back through our own driver proves only that
//! this crate is self-consistent — the same misunderstanding of where a
//! block lives would place the write and then find it again. So the
//! bytes are read back **by the Linux kernel**, through its own XFS
//! driver, and compared to a hash computed before the image was touched.
//!
//! And a write that landed in the right place could still have damaged
//! something else: an off-by-one that ran past an extent would corrupt
//! whatever came next, which the file's own contents would never reveal.
//! So `xfs_repair` inspects the whole filesystem afterwards and has to
//! find nothing wrong.
//!
//! The write itself happens on the host, because the driver is Rust and
//! the oracle VM has no Rust toolchain; the verification happens in the
//! VM, which is where the tooling and a Linux kernel are. `scripts/vm.sh`
//! bridges the two.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Generate fixtures with
//! `./scripts/vm-build-data-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// The file the write lands in: 8 MiB of random bytes, so it is
/// extent-backed, fully written, not sparse and not shared — every
/// condition the in-place path requires.
const TARGET: &str = "/large.bin";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Run a shell snippet inside the oracle VM, returning its stdout.
fn vm_run(script: &str) -> Option<String> {
    let out = Command::new(repo().join("scripts/vm.sh"))
        .arg("run")
        .arg(script)
        .output()
        .ok()?;
    if !out.status.success() {
        eprintln!(
            "vm.sh run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A working image in the shared folder, removed when it goes out of
/// scope — including on a panic.
///
/// It has to live in `.vm-share` for the VM to see it, and every other
/// suite in this crate treats each `.img` there as a fixture to check.
/// A working copy left behind is therefore not merely untidy: it fails
/// unrelated tests, which is exactly what happened the first time this
/// suite ran.
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

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// The inode at `path`, together with its on-disk bytes — the pair the
/// read and write paths both take.
fn resolve(fs: &Filesystem, path: &str) -> Option<(fs_xfs::inode::Inode, Vec<u8>)> {
    let inode = fs.lookup_path(path).ok()?;
    fs.read_inode_raw(inode.ino).ok()
}

/// Overwrite a run of bytes, then have Linux confirm both that the file
/// reads back correctly and that nothing else on the volume was harmed.
#[test]
fn an_in_place_write_survives_the_kernel_and_the_checker() {
    let source = share().join("xfsdata-default.img");
    if !source.exists() {
        eprintln!("no xfsdata-default fixture — skipping");
        return;
    }
    // A separate image, so a failure leaves the fixtures usable and a
    // rerun starts from the same state.
    let scratch = Scratch::from(&source, "xfswrite.img");
    let img = scratch.path();

    // What the file holds now, and what it should hold afterwards.
    // Computed before anything is written, from the untouched copy.
    let (offset, payload) = (4096u64, b"in-place write, no metadata touched\n".repeat(8));
    let expected = {
        let dev = FileDevice::open(img).expect("open read-only");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
        let Some((inode, raw)) = resolve(&fs, TARGET) else {
            eprintln!("fixture has no {TARGET} — skipping");
            return;
        };
        let mut whole = fs.read_file(&inode, &raw).expect("read the file");
        assert!(
            offset as usize + payload.len() < whole.len(),
            "the payload must land inside the file, not extend it"
        );
        whole[offset as usize..offset as usize + payload.len()].copy_from_slice(&payload);
        sha256_hex(&whole)
    };

    // The write.
    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let (inode, raw) = resolve(&fs, TARGET).expect("resolve the target");
        let n = fs
            .write_at(&inode, &raw, offset, &payload)
            .expect("the write must be accepted");
        assert_eq!(n, payload.len(), "a short write should not be possible");
    }

    // Verification, in the VM. `xfs_repair` cannot work on a file in the
    // shared folder — it wants the host filesystem's geometry and gets
    // ENOTDIR from 9p — so it runs on a copy in the guest.
    let script = format!(
        r#"
        set -e
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/xfswrite.img "$img"
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 || true
        echo "REPAIR_END"
        mnt=$(mktemp -d)
        mount -o ro,loop "$img" "$mnt"
        echo "SHA $(sha256sum "$mnt{TARGET}" | cut -d' ' -f1)"
        umount "$mnt"; rmdir "$mnt"; rm -f "$img"
        "#
    );
    let Some(out) = vm_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    // The kernel's reading of the file we just wrote.
    let got = out
        .lines()
        .find_map(|l| l.strip_prefix("SHA "))
        .unwrap_or_else(|| panic!("the VM did not report a hash:\n{out}"))
        .trim();
    assert_eq!(
        got, expected,
        "the kernel reads back different bytes than were written\n{out}"
    );

    // And the volume as a whole is still sound. `-n` makes no changes, so
    // anything it reports is a real finding rather than a repair.
    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    for bad in [
        "valuable metadata changes in a log",
        "corrupt",
        "bad ",
        "would fix",
        "would reset",
        "would rebuild",
        "inconsistent",
    ] {
        assert!(
            !report.to_lowercase().contains(bad),
            "the checker objected after an in-place write ({bad:?}):\n{report}"
        );
    }
}

/// The refusals, against a real filesystem rather than a constructed
/// inode: a read-only mount must decline, and it must decline before
/// touching the device.
#[test]
fn a_read_only_mount_of_a_real_volume_refuses_to_write() {
    let source = share().join("xfsdata-default.img");
    if !source.exists() {
        eprintln!("no xfsdata-default fixture — skipping");
        return;
    }
    let scratch = Scratch::from(&source, "xfswrite-ro.img");
    let img = scratch.path();
    let before = {
        let d = FileDevice::open(img).expect("open");
        let mut b = vec![0u8; 4096];
        d.read_at(0, &mut b).expect("read");
        sha256_hex(&b)
    };

    let dev = FileDevice::open(img).expect("open read-only");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount read-only");
    let (inode, raw) = resolve(&fs, TARGET).expect("resolve the target");
    let err = fs
        .write_at(&inode, &raw, 0, b"nope")
        .expect_err("a read-only mount must refuse");
    assert!(matches!(err, fs_xfs::Error::ReadOnly), "got {err}");

    let after = {
        let d = FileDevice::open(img).expect("open");
        let mut b = vec![0u8; 4096];
        d.read_at(0, &mut b).expect("read");
        sha256_hex(&b)
    };
    assert_eq!(before, after, "a refused write still changed the image");
}

/// Changing an inode's timestamps and permissions must be visible to
/// Linux and must leave the volume sound.
///
/// The same two-sided check as the data write, for the same reason:
/// reading the change back through this driver would prove only that it
/// wrote what it meant to, not that Linux agrees the result is an inode.
/// A wrong CRC, a wrong timestamp encoding, or a field written at the
/// wrong offset would all read back perfectly here and fail there.
#[test]
fn an_attribute_change_survives_the_kernel_and_the_checker() {
    use fs_xfs::inode::Timestamp;
    use fs_xfs::write::AttrChange;

    let source = share().join("xfsdata-default.img");
    if !source.exists() {
        eprintln!("no xfsdata-default fixture — skipping");
        return;
    }
    let scratch = Scratch::from(&source, "xfsattr.img");
    let img = scratch.path();

    // A time far enough from any the fixture already holds that a field
    // written to the wrong offset cannot coincidentally look right, and
    // a permission set no fixture file uses.
    let when = Timestamp {
        sec: 1_700_000_000,
        nsec: 123_456_789,
    };
    let perms = 0o741u16;

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let (inode, _) = resolve(&fs, TARGET).expect("resolve the target");
        fs.set_attributes(
            &inode,
            &AttrChange {
                permissions: Some(perms),
                mtime: Some(when),
                ..Default::default()
            },
        )
        .expect("the attribute change must be accepted");
    }

    let script = format!(
        r#"
        set -e
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/xfsattr.img "$img"
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 || true
        echo "REPAIR_END"
        mnt=$(mktemp -d)
        mount -o ro,loop "$img" "$mnt"
        echo "MODE $(stat -c%a "$mnt{TARGET}")"
        echo "MTIME $(stat -c%Y "$mnt{TARGET}")"
        echo "MTIME_NS $(stat -c%y "$mnt{TARGET}")"
        umount "$mnt"; rmdir "$mnt"; rm -f "$img"
        "#
    );
    let Some(out) = vm_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    let field = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("the VM did not report {k}:\n{out}"))
            .trim()
            .to_string()
    };

    assert_eq!(
        field("MODE"),
        format!("{perms:o}"),
        "the kernel sees different permissions than were written\n{out}"
    );
    assert_eq!(
        field("MTIME"),
        when.sec.to_string(),
        "the kernel sees a different modification time than was written\n{out}"
    );
    // The nanoseconds matter separately: a timestamp encoded in the wrong
    // representation can land on the right second and the wrong fraction.
    assert!(
        field("MTIME_NS").contains("123456789"),
        "the sub-second part was not stored: {}\n{out}",
        field("MTIME_NS")
    );

    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    for bad in [
        "valuable metadata changes in a log",
        "corrupt",
        "bad ",
        "would fix",
        "would reset",
        "would rebuild",
        "inconsistent",
    ] {
        assert!(
            !report.to_lowercase().contains(bad),
            "the checker objected after an attribute change ({bad:?}):\n{report}"
        );
    }
}

/// Shortening a file must be visible to Linux and leave the volume sound.
///
/// The interesting part is not the size — it is that the file keeps its
/// blocks, so the inode ends up claiming fewer bytes than it has space
/// for. That state has to be one the checker accepts and the kernel
/// reads correctly, and neither can be established from this side.
#[test]
fn a_truncate_survives_the_kernel_and_the_checker() {
    use fs_xfs::inode::Timestamp;

    let source = share().join("xfsdata-default.img");
    if !source.exists() {
        eprintln!("no xfsdata-default fixture — skipping");
        return;
    }
    let scratch = Scratch::from(&source, "xfstrunc.img");
    let img = scratch.path();

    // Deliberately not block-aligned, so the partial-block tail has to be
    // cleared rather than left holding what it held.
    let new_size = 5000u64;
    let when = Timestamp {
        sec: 1_600_000_000,
        nsec: 0,
    };

    let old_size = {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let (inode, _) = resolve(&fs, TARGET).expect("resolve the target");
        let old = inode.size;
        assert!(
            old > new_size,
            "the fixture file must be longer than the target"
        );
        fs.truncate(&inode, new_size, Some(when))
            .expect("the truncate must be accepted");
        old
    };

    let script = format!(
        r#"
        set -e
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/xfstrunc.img "$img"
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 || true
        echo "REPAIR_END"
        mnt=$(mktemp -d)
        mount -o ro,loop "$img" "$mnt"
        echo "SIZE $(stat -c%s "$mnt{TARGET}")"
        echo "BLOCKS $(stat -c%b "$mnt{TARGET}")"
        echo "MTIME $(stat -c%Y "$mnt{TARGET}")"
        umount "$mnt"; rmdir "$mnt"; rm -f "$img"
        "#
    );
    let Some(out) = vm_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    let field = |k: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{k} ")))
            .unwrap_or_else(|| panic!("the VM did not report {k}:\n{out}"))
            .trim()
            .to_string()
    };

    assert_eq!(
        field("SIZE"),
        new_size.to_string(),
        "the kernel sees a different size than was written\n{out}"
    );
    assert_eq!(
        field("MTIME"),
        when.sec.to_string(),
        "the modification time did not move with the truncate\n{out}"
    );

    // The blocks are deliberately still there. Asserting it keeps the
    // limitation honest: if a later change starts freeing them, this
    // fails and the documentation has to be revisited with it.
    let blocks: u64 = field("BLOCKS").parse().expect("a block count");
    assert!(
        blocks * 512 >= old_size,
        "the file's blocks were freed, which this path does not do and cannot do \
         without the log — {} bytes of blocks for a file that was {old_size}\n{out}",
        blocks * 512
    );

    let report: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    for bad in [
        "valuable metadata changes in a log",
        "corrupt",
        "bad ",
        "would fix",
        "would reset",
        "would rebuild",
        "inconsistent",
    ] {
        assert!(
            !report.to_lowercase().contains(bad),
            "the checker objected after a truncate ({bad:?}):\n{report}"
        );
    }
}
