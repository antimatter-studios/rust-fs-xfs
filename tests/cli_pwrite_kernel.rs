//! `fs.xfs write --at`, judged by THE KERNEL and by xfsprogs.
//!
//! Linux preallocates a file on a copy of the kernel-made v5 CLI image in
//! the harness guest: `fallocate` leaves one unwritten extent. `fs.xfs
//! write --at` is then refused a hole, that unwritten extent and a write
//! past the end of a file (exit 3), each leaving the image byte-identical,
//! and patches files in place: inside one block, across a block boundary,
//! over several blocks, ending at the end of a file, and in a file whose
//! extents are in a B+tree.
//!
//! THE EXPECTED BYTES ARE THE KERNEL'S, NOT OURS. The guest copies every
//! file off a kernel mount of the image as it was before the first patch
//! and applies the same patches with `dd conv=notrunc`, which is
//! `pwrite(2)`. `xfs_repair -n` then grades the patched image as the tool
//! left it, before anything has replayed its log, and the kernel mounts it
//! and `cmp`s each file against those expected bytes.
//!
//! EVERY GUEST STEP WORKS ON A COPY ON THE GUEST'S OWN DISK, for the
//! reason `cli_write_kernel.rs` gives: a loop mount of a file on the
//! shared folder mixes the guest's page cache with the host's view of it.

mod cli_support;
mod common;

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Output, Stdio};

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_pwrite_kernel";

/// `fs.xfs IMAGE write --at AT PATH` with `bytes` on stdin.
fn write_at(image: &str, path: &str, at: u64, bytes: &[u8]) -> Output {
    let mut child = tool("fs.xfs")
        .args([image, "write", "--at", &at.to_string(), path])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fs.xfs write --at");
    // A tool that refuses its command line closes stdin unread; its status
    // and its error are the answer, not the broken pipe.
    let _ = child.stdin.take().expect("stdin").write_all(bytes);
    child
        .wait_with_output()
        .expect("wait for fs.xfs write --at")
}

/// The SHA-256 of the file at `path`, read in pieces: an image is
/// hundreds of MiB, almost all of it holes.
fn digest(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut file = std::fs::File::open(path).expect("open the image");
    let mut hash = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let got = file.read(&mut buf).expect("read the image");
        if got == 0 {
            break;
        }
        hash.update(&buf[..got]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn write_at_patches_in_place_what_the_kernel_reads_back_and_xfs_repair_accepts() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-v5.img", std::process::id()),
    );
    let image = copy.path().to_str().unwrap().to_string();
    let image = image.as_str();

    let made = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        fallocate -l 65536 "$work/m/prealloc" && echo PREALLOCATED
        umount "$work/m"
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(image),
    ));
    assert!(
        made.contains("PREALLOCATED"),
        "the kernel did not preallocate /prealloc:\n{made}"
    );
    let before = scratch::Volume::copy_of(
        SUITE,
        copy.path(),
        &format!("{}-before.img", std::process::id()),
    );

    let bs: u64 = stdout(&ok(tool("fs.xfs").args([
        image,
        "get",
        "block_size",
        "--text",
    ])))
    .trim()
    .parse()
    .expect("block_size is a number");

    // Refused, with the image left exactly as it was.
    let untouched = digest(copy.path());
    for (path, at, len, why) in [
        ("/sparse.bin", 0, 10, "hole"),
        ("/prealloc", 0, 10, "unwritten"),
        ("/prealloc", bs - 5, 10, "unwritten"),
        ("/small.txt", 10, 5, "past the end"),
    ] {
        let out = write_at(image, path, at, &pattern(len, 9));
        assert_eq!(
            out.status.code(),
            Some(3),
            "write --at {at} {path} was not refused as a shape (exit 3): {}",
            stderr(&out)
        );
        assert!(
            stdout(&out).is_empty(),
            "a refusal printed a report: {}",
            stdout(&out)
        );
        assert!(
            stderr(&out).contains(why),
            "write --at {at} {path} was refused, but not for its {why}: {}",
            stderr(&out)
        );
    }
    assert_eq!(
        digest(copy.path()),
        untouched,
        "a refused write --at changed the image"
    );

    let large: u64 = 8 << 20;
    let patches: Vec<(&str, u64, Vec<u8>)> = vec![
        ("/small.txt", 0, b"HELLO".to_vec()),
        ("/medium.bin", 100, pattern(50, 1)),
        ("/medium.bin", bs - 6, pattern(20, 2)),
        ("/medium.bin", 3 * bs + 7, pattern(3 * bs as usize, 3)),
        ("/large.bin", large - 10, pattern(10, 4)),
        ("/large.bin", large - bs - 3, pattern(bs as usize + 3, 5)),
        ("/fragmented.bin", 2 * bs + 7, pattern(100, 6)),
    ];
    let mut patch_files = Vec::new();
    let mut apply = String::new();
    for (i, (path, at, bytes)) in patches.iter().enumerate() {
        let size = json_field(
            &stdout(&ok(tool("fs.xfs").args([image, "stat", path]))),
            "size",
        );
        let out = write_at(image, path, *at, bytes);
        assert!(
            out.status.success(),
            "write --at {at} {path} failed ({:?}): {}",
            out.status.code(),
            stderr(&out)
        );
        let report = stdout(&out);
        assert_eq!(json_field(&report, "path"), *path, "{report}");
        assert_eq!(json_field(&report, "at"), at.to_string(), "{report}");
        assert_eq!(
            json_field(&report, "bytes"),
            bytes.len().to_string(),
            "{report}"
        );
        assert_eq!(json_field(&report, "size"), size, "{report}");

        let patch = scratch::Volume::empty(SUITE, &format!("{}-patch-{i}", std::process::id()), 0);
        std::fs::write(patch.path(), bytes).expect("write the patch for the guest");
        apply.push_str(&format!(
            "dd if={patch} of=\"$work/expect{path}\" bs=65536 seek={at} oflag=seek_bytes \
             conv=notrunc status=none\n",
            patch = guest_quote(patch.path().to_str().unwrap()),
        ));
        patch_files.push(patch);
    }
    let dirty = stdout(&ok(tool("fs.xfs").args([image, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "write --at left the log dirty");

    let files = "small.txt medium.bin large.bin fragmented.bin sparse.bin prealloc";
    let out = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {before} "$work/before"
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m" "$work/expect"
        mount -o loop,ro,nouuid "$work/before" "$work/m"
        for f in {files}; do cp "$work/m/$f" "$work/expect/$f"; done
        umount "$work/m"
        {apply}
        {repair}
        if mount -o loop,nouuid "$work/image" "$work/m"; then
            echo MOUNT_OK
            for f in {files}; do
                if cmp "$work/expect/$f" "$work/m/$f"; then echo "SAME $f"; else echo "DIFFERENT $f"; fi
            done
            umount "$work/m" || echo UMOUNT_FAILED
        else
            echo MOUNT_REFUSED
        fi
        echo DONE
        "#,
        before = guest_quote(before.path().to_str().unwrap()),
        image = guest_quote(image),
        repair = repair::script("\"$work/image\""),
    ));
    repair::assert_agreed(&out, "the volume write --at patched, before any replay");
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount it:\n{out}"
    );
    for f in files.split(' ') {
        assert!(
            out.lines().any(|l| l == format!("SAME {f}")),
            "{f}: the kernel reads different bytes from the ones pwrite(2) gives the \
             same patches:\n{out}"
        );
    }
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    drop(patch_files);
}
