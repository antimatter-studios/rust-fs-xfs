//! Renames over existing targets read back in the Linux kernel as POSIX
//! says, and refused ones change nothing (#383).
//!
//! The kernel makes every file here, including a hard-linked pair, which
//! this driver cannot make yet (#384):
//!
//! - `a/new` (8 KiB of 0x11) over `b/old` (64 KiB of 0x22): a file with data
//!   over a file with data, freed with its blocks;
//! - `a/other` over `b/linked`, one name of a pair whose other name is
//!   `b/twin`: the inode survives with one link;
//! - the directory `a/sub`, holding `inside`, over the empty `b/empty`.
//!
//! The kernel mounts the result, which replays the record, and must list
//! `b` as it should be, read `b/old` as what `a/new` held, give `b/twin`
//! one link, and find `inside` under `b/empty`; `xfs_repair -n` must call
//! the volume clean. A second volume is asked for the three renames POSIX
//! forbids, each refused, and its hash must not change.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::{Error, Filesystem};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;

const SUITE: &str = "rename_over_target_oracle";

fn built(tag: &str) -> scratch::Volume {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-{tag}.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        mkdir "$m/a" "$m/b" "$m/a/sub" "$m/b/empty" "$m/c" "$m/c/dir" "$m/c/full"
        xfs_io -f -c 'pwrite -q -S 0x11 0 8k' "$m/a/new"
        xfs_io -f -c 'pwrite -q -S 0x22 0 64k' "$m/b/old"
        xfs_io -f -c 'pwrite -q -S 0x33 0 4k' "$m/a/other"
        xfs_io -f -c 'pwrite -q -S 0x44 0 4k' "$m/b/linked"
        ln "$m/b/linked" "$m/b/twin"
        : > "$m/a/sub/inside"
        : > "$m/c/file"
        : > "$m/c/full/x"
        sync
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        out.contains("MKFS_OK") && out.contains("MOUNT_OK"),
        "building the volume failed:\n{out}"
    );
    volume
}

fn open_rw(volume: &scratch::Volume) -> Filesystem {
    let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
    Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn image_hash(path: &std::path::Path) -> String {
    let mut file = std::fs::File::open(path).expect("open");
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).expect("read");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
}

#[test]
fn the_kernel_reads_renames_over_existing_targets() {
    let volume = built("replace");
    {
        let fs = open_rw(&volume);
        let dir = |p: &str| fs.lookup_path(p).expect(p).ino;
        let (a, b) = (dir("/a"), dir("/b"));
        fs.rename(a, b"new", b, b"old")
            .expect("a file over a file with data");
        fs.rename(a, b"other", b, b"linked")
            .expect("a file over one of two links");
        fs.rename(a, b"sub", b, b"empty")
            .expect("a directory over an empty one");
    }
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            ls -1 "$m/a" | sed 's|^|NAME a |'
            ls -1 "$m/b" | sed 's|^|NAME b |'
            echo "OLD $(stat -c %s "$m/b/old") $(sha256sum < "$m/b/old" | cut -d' ' -f1)"
            echo "TWIN_LINKS $(stat -c %h "$m/b/twin")"
            [ -e "$m/b/empty/inside" ] && echo INSIDE_PRESENT || echo INSIDE_ABSENT
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
    repair::assert_agreed(&out, "the volume after renames over existing targets");
    let names = |dir: &str| -> Vec<String> {
        let mut v: Vec<String> = out
            .lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("NAME {dir} ")))
            .map(str::to_string)
            .collect();
        v.sort();
        v
    };
    assert!(names("a").is_empty(), "a still lists names:\n{out}");
    assert_eq!(names("b"), ["empty", "linked", "old", "twin"], "b:\n{out}");
    let want = format!("OLD 8192 {:x}", Sha256::digest(vec![0x11u8; 8192]));
    assert!(
        out.lines().any(|l| l.trim() == want),
        "b/old is not a/new:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim() == "TWIN_LINKS 1"),
        "b/twin's links:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim() == "INSIDE_PRESENT"),
        "b/empty is not a/sub:\n{out}"
    );
}

#[test]
fn renames_posix_forbids_leave_the_volume_byte_for_byte() {
    let volume = built("refuse");
    let before = image_hash(volume.path());
    {
        let fs = open_rw(&volume);
        let c = fs.lookup_path("/c").expect("c").ino;
        let file_over_dir = fs.rename(c, b"file", c, b"dir");
        assert!(
            matches!(file_over_dir, Err(Error::NotAFile)),
            "{file_over_dir:?}"
        );
        let dir_over_file = fs.rename(c, b"dir", c, b"file");
        assert!(
            matches!(dir_over_file, Err(Error::NotADirectory)),
            "{dir_over_file:?}"
        );
        let over_full = fs.rename(c, b"dir", c, b"full");
        assert!(
            matches!(over_full, Err(Error::DirectoryNotEmpty)),
            "{over_full:?}"
        );
    }
    assert_eq!(
        image_hash(volume.path()),
        before,
        "a refused rename changed the image"
    );
}
