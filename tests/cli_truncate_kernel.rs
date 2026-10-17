//! The CLI's truncates are clean before replay, free blocks, and leave
//! exactly the lengths and zero-filled bytes the Linux kernel reads.
mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_truncate_kernel";

#[test]
fn truncated_files_are_clean_and_the_kernel_reads_their_lengths_and_zero_tails() {
    let image = scratch::Volume::empty(SUITE, "lengths.img", 320 * 1024 * 1024);
    let img = image.path().to_str().unwrap();
    let built = kernel_run(&format!(
        r#"
        set -eu
        m=$(mktemp -d)
        trap 'mountpoint -q "$m" && umount "$m"; rmdir "$m"' EXIT
        mkfs.xfs -q -f {image}
        mount -o loop {image} "$m"
        for f in zero boundary grow regrow; do
            xfs_io -f -c 'pwrite -q -S 0x51 0 64k' -c fsync "$m/$f"
        done
        umount "$m" || echo UMOUNT_FAILED
        echo BUILT_OK
        echo DONE
        "#,
        image = guest_quote(img),
    ));
    assert!(built.contains("BUILT_OK"), "{built}");
    let free_before = stdout(&ok(tool("fs.xfs").args([
        img,
        "get",
        "free_bytes",
        "--text",
    ])))
    .trim()
    .parse::<u64>()
    .unwrap();
    for (path, size) in [
        ("/zero", "0"),
        ("/boundary", "4196"),
        ("/grow", "73733"),
        ("/regrow", "4196"),
        ("/regrow", "16384"),
    ] {
        let out = ok(tool("fs.xfs").args([img, "truncate", path, size]));
        assert_eq!(json_field(&stdout(&out), "path"), path);
        assert_eq!(json_field(&stdout(&out), "size"), size);
        assert_eq!(
            stdout(&ok(tool("fs.xfs").args([img, "get", "dirty", "--text"]))).trim(),
            "false"
        );
    }
    let free_after = stdout(&ok(tool("fs.xfs").args([
        img,
        "get",
        "free_bytes",
        "--text",
    ])))
    .trim()
    .parse::<u64>()
    .unwrap();
    assert!(free_after > free_before, "shrink did not release blocks");
    let mut grow = vec![0x51; 65536];
    grow.resize(73733, 0);
    let mut regrow = vec![0x51; 4196];
    regrow.resize(16384, 0);
    let expected = [
        ("zero", Vec::new()),
        ("boundary", vec![0x51; 4196]),
        ("grow", grow),
        ("regrow", regrow),
    ];
    let out = kernel_run(&format!(
        r#"
        set -eu
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        {repair}
        mkdir "$work/m"
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        for f in zero boundary grow regrow; do
            echo "FILE $f $(stat -c %s "$work/m/$f") $(sha256sum < "$work/m/$f" | cut -d' ' -f1)"
        done
        test "$(stat -c %b "$work/m/zero")" = 0
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(img),
        repair = repair::script("\"$work/image\""),
    ));
    repair::assert_agreed(&out, "CLI truncates before a kernel mount");
    for (name, bytes) in expected {
        let line = format!("FILE {name} {} {}", bytes.len(), sha256_hex(&bytes));
        assert!(out.lines().any(|l| l.trim() == line), "want {line}\n{out}");
    }
}

#[test]
fn a_cut_inside_a_shared_block_is_refused_without_changing_either_file() {
    let image = scratch::Volume::empty(SUITE, "shared.img", 320 * 1024 * 1024);
    let img = image.path().to_str().unwrap();
    let built = kernel_run(&format!(
        r#"
        set -eu
        m=$(mktemp -d)
        trap 'mountpoint -q "$m" && umount "$m"; rmdir "$m"' EXIT
        mkfs.xfs -q -f -m reflink=1 {image}
        mount -o loop {image} "$m"
        xfs_io -f -c 'pwrite -q -S 0x51 0 64k' -c fsync "$m/source"
        cp --reflink=always "$m/source" "$m/copy"
        umount "$m" || echo UMOUNT_FAILED
        echo BUILT_OK
        echo DONE
        "#,
        image = guest_quote(img),
    ));
    assert!(built.contains("BUILT_OK"), "{built}");
    let before = sha256_hex(&std::fs::read(img).unwrap());
    let out = tool("fs.xfs")
        .args([img, "truncate", "/copy", "4196"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("reflinked"), "{}", stderr(&out));
    assert_eq!(sha256_hex(&std::fs::read(img).unwrap()), before);
    let out = kernel_run(&format!(
        r#"
        set -eu
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        {repair}
        mkdir "$work/m"
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        for f in source copy; do
            test "$(stat -c %s "$work/m/$f")" = 65536
            test "$(sha256sum < "$work/m/$f" | cut -d' ' -f1)" = {hash}
        done
        umount "$work/m" || echo UMOUNT_FAILED
        echo SHARED_OK
        echo DONE
        "#,
        image = guest_quote(img),
        repair = repair::script("\"$work/image\""),
        hash = sha256_hex(&vec![0x51; 65536]),
    ));
    repair::assert_agreed(&out, "the refused CLI truncate");
    assert!(out.contains("SHARED_OK"), "{out}");
}
