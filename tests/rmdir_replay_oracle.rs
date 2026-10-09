//! A directory this driver removes is gone to the Linux kernel, and a
//! refused removal changes nothing (#385).
//!
//! The volume is made by `mkfs.xfs -p` in the harness guest, so the
//! directories are the kernel tool's and not this driver's: `empty`, an
//! empty directory; `full`, holding `inside`; and `file`, a regular file.
//!
//! - **Removal.** The driver removes `empty` and stops. The kernel then
//!   mounts the image, which replays the record, and must find the name
//!   gone and the root's link count one lower, since `empty`'s `..` was a
//!   link to it. A `mkdir` in the kernel afterwards must succeed, and
//!   `xfs_repair -n` must call the volume clean.
//! - **Refusal.** The driver is asked to remove `full` and `file`, and
//!   must refuse each with the documented error and leave the image byte
//!   for byte as it was: its SHA-256 before and after must be equal, and
//!   `xfs_repair -n` must still call it clean.

mod common;

use common::{kernel_run, oracle, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::{Error, Filesystem};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;

const SUITE: &str = "rmdir_replay_oracle";

/// A kernel-made image holding `empty/`, `full/inside` and `file`.
fn base_image(tag: &str) -> scratch::Volume {
    let dir = scratch::dir(SUITE);
    let none = dir.join(format!("{tag}-none"));
    std::fs::write(&none, b"").unwrap();
    let proto = dir.join(format!("{tag}-proto"));
    std::fs::write(
        &proto,
        format!(
            "/dev/null\n0 0\nd--755 0 0\n\
             empty d--755 0 0\n$\n\
             full d--755 0 0\ninside ---644 0 0 {n}\n$\n\
             file ---644 0 0 {n}\n$\n",
            n = none.display()
        ),
    )
    .unwrap();
    let image = scratch::Volume::empty(SUITE, &format!("{tag}-base.img"), 300 * 1024 * 1024);
    let out = oracle("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(&proto)
        .arg(image.path())
        .output();
    assert!(out.ok(), "mkfs.xfs: {}{}", out.stdout, out.stderr);
    image
}

fn open_rw(volume: &scratch::Volume) -> Filesystem {
    let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
    Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn image_hash(path: &std::path::Path) -> String {
    let mut file = std::fs::File::open(path).expect("open the image");
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).expect("read the image");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
}

/// Mount a copy of `volume` in the kernel, run `probe` against `$m`, and
/// have `xfs_repair -n` judge the copy.
fn in_kernel(volume: &scratch::Volume, probe: &str) -> String {
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {probe}
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
        "the kernel refused the volume:\n{out}"
    );
    out
}

fn has(out: &str, line: &str) -> bool {
    out.lines().any(|l| l.trim() == line)
}

#[test]
fn the_kernel_agrees_a_directory_this_driver_removed_is_gone() {
    let volume = base_image("remove");
    let before = in_kernel(&volume, r#"echo "ROOT_LINKS $(stat -c %h "$m")""#);
    repair::assert_agreed(&before, "the kernel-made volume before removal");
    {
        let fs = open_rw(&volume);
        let root = fs.superblock().rootino;
        fs.remove_directory(root, b"empty")
            .expect("remove the empty directory");
    }
    let after = in_kernel(
        &volume,
        r#"echo "ROOT_LINKS $(stat -c %h "$m")"
            [ -e "$m/empty" ] && echo EMPTY_PRESENT || echo EMPTY_ABSENT
            [ -e "$m/full/inside" ] && echo INSIDE_PRESENT || echo INSIDE_ABSENT
            mkdir "$m/again" && echo MKDIR_OK"#,
    );
    repair::assert_agreed(&after, "the volume after the driver removed a directory");
    let links = |out: &str| -> u32 {
        out.lines()
            .find_map(|l| l.trim().strip_prefix("ROOT_LINKS "))
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or_else(|| panic!("no root link count:\n{out}"))
    };
    assert!(
        has(&after, "EMPTY_ABSENT"),
        "the kernel still sees the directory:\n{after}"
    );
    assert!(
        has(&after, "INSIDE_PRESENT"),
        "a sibling directory lost its entry:\n{after}"
    );
    assert_eq!(
        links(&after),
        links(&before) - 1,
        "the root kept the link the removed directory's .. held:\n{after}"
    );
    assert!(
        has(&after, "MKDIR_OK"),
        "the kernel cannot make a directory afterwards:\n{after}"
    );
}

#[test]
fn a_refused_removal_leaves_the_volume_byte_for_byte() {
    let volume = base_image("refuse");
    let before = image_hash(volume.path());
    {
        let fs = open_rw(&volume);
        let root = fs.superblock().rootino;
        let full = fs.remove_directory(root, b"full");
        assert!(
            matches!(full, Err(Error::DirectoryNotEmpty)),
            "full: {full:?}"
        );
        let file = fs.remove_directory(root, b"file");
        assert!(matches!(file, Err(Error::NotADirectory)), "file: {file:?}");
    }
    assert_eq!(
        image_hash(volume.path()),
        before,
        "a refused removal changed the image"
    );
    let out = in_kernel(&volume, "true");
    repair::assert_agreed(&out, "the volume after two refused removals");
}
