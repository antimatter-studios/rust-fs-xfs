//! What `mkfs.xfs` makes is a filesystem the reference tools and the
//! Linux kernel accept, in the shape the options asked for (#338).
//!
//! Three independent judges, in the harness guest, for every volume:
//!
//! - `xfs_repair -n` on the volume exactly as `mkfs.xfs` left it, before
//!   anything else has touched it. A fresh filesystem has a clean log, so
//!   the tool grades every structure rather than declining to look;
//! - the kernel mounts it, makes a directory and writes a file into it,
//!   reads the file back and unmounts, and `xfs_repair -n` is asked again
//!   about what the kernel left;
//! - `xfs_db` reports the geometry the options asked for: the label, the
//!   block size, the allocation group count.
//!
//! Then this crate's own reader opens the volume the kernel wrote to and
//! reads the kernel's file back, which closes the loop the other way.
//!
//! EVERY GUEST STEP WORKS ON A COPY ON THE GUEST'S OWN DISK, for the reason
//! `cli_write_kernel` gives: a loop mount of a file on the shared folder
//! mixes two page caches, and `xfs_repair` gets ENOTDIR from it.

mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_mkfs_kernel";
const MIB: u64 = 1024 * 1024;

/// An empty sparse device of `bytes`, formatted by our `mkfs.xfs` with
/// `args`.
fn formatted(name: &str, bytes: u64, args: &[&str]) -> scratch::Volume {
    let volume =
        scratch::Volume::empty(SUITE, &format!("{}-{name}.img", std::process::id()), bytes);
    let image = volume.path().to_str().unwrap();
    let out = tool("mkfs.xfs")
        .args(args)
        .arg(image)
        .output()
        .expect("spawn mkfs.xfs");
    assert!(
        out.status.success(),
        "mkfs.xfs {args:?} on a {bytes}-byte device failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(&out),
        stderr(&out)
    );
    volume
}

/// `xfs_repair -n` and `xfs_db` on the volume as it stands, then a kernel
/// mount that writes `/d/hello` and reads it back, then `xfs_repair -n`
/// again. Everything is printed; the caller reads it.
fn judged(image: &str, hello: &[u8]) -> (String, String) {
    let first = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        img=$(mktemp -u /tmp/cli-mkfs-XXXXXX.img)
        cp --sparse=always {image} "$img"
        xfs_db -r -c 'sb 0' -c 'p blocksize agcount dblocks' -c label "$img" | sed 's/^/DB /'
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    let hex: String = hello.iter().map(|b| format!("\\x{b:02x}")).collect();
    let second = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        img=$(mktemp -u /tmp/cli-mkfs-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop "$img" "$m"; then
            echo MOUNT_OK
            mkdir "$m/d" && printf '{hex}' > "$m/d/hello" && sync
            sha256sum "$m/d/hello" | sed 's/ .*//; s/^/SUM /'
            df -B1 --output=size "$m" | tail -1 | sed 's/^ */SIZE /'
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_REFUSED
            dmesg | tail -20
        fi
        rmdir "$m"
        cp --sparse=always "$img" {image}
        {repair}
        rm -f "$img"
        sync
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    (first, second)
}

/// The value `xfs_db` printed for `field`.
#[track_caller]
fn db(out: &str, field: &str) -> String {
    let prefix = format!("DB {field} = ");
    out.lines()
        .find_map(|l| l.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("xfs_db printed no {field}:\n{out}"))
        .trim()
        .to_string()
}

/// One volume, judged from every side.
#[track_caller]
fn accepted(name: &str, bytes: u64, args: &[&str], label: &str, blocksize: u32) -> String {
    let volume = formatted(name, bytes, args);
    let image = volume.path().to_str().unwrap().to_string();
    let hello = pattern(10_000, bytes as u32);
    let (fresh, used) = judged(&image, &hello);

    repair::assert_agreed(&fresh, &format!("{name}: the volume mkfs.xfs made"));
    assert_eq!(db(&fresh, "label"), format!("\"{label}\""), "{fresh}");
    assert_eq!(db(&fresh, "blocksize"), blocksize.to_string(), "{fresh}");
    let dblocks: u64 = db(&fresh, "dblocks").parse().unwrap();
    assert_eq!(
        dblocks,
        bytes / u64::from(blocksize),
        "{name}: the filesystem does not cover the device:\n{fresh}"
    );

    assert!(
        used.contains("MOUNT_OK"),
        "{name}: the kernel would not mount what mkfs.xfs made:\n{used}"
    );
    assert!(
        used.contains(&format!("SUM {}", sha256_hex(&hello))),
        "{name}: the kernel read back different bytes from those it wrote:\n{used}"
    );
    repair::assert_agreed(
        &used,
        &format!("{name}: the volume after the kernel wrote to it"),
    );

    // And this crate reads what the kernel wrote.
    let read = ok(tool("fs.xfs").args([image.as_str(), "read", "/d/hello"]));
    assert!(
        read.stdout == hello,
        "{name}: fs.xfs reads different bytes from those the kernel wrote"
    );
    let got = stdout(&ok(tool("fs.xfs").args([
        image.as_str(),
        "get",
        "label",
        "--text",
    ])));
    assert_eq!(got.trim(), label, "{name}: fs.xfs reads a different label");
    fresh
}

#[test]
fn a_default_volume_is_one_the_kernel_and_xfs_repair_accept() {
    accepted("default-512m", 512 * MIB, &["-L", "DJMKFS"], "DJMKFS", 4096);
}

#[test]
fn volumes_of_several_sizes_are_accepted() {
    for (name, bytes) in [
        ("1g", 1024 * MIB),
        ("5g", 5 * 1024 * MIB),
        ("40g", 40 * 1024 * MIB),
    ] {
        accepted(name, bytes, &["-L", "SIZES"], "SIZES", 4096);
    }
}

#[test]
fn the_block_size_and_ag_count_asked_for_are_the_ones_made() {
    for bs in [1024u32, 2048, 4096] {
        let size = format!("size={bs}");
        accepted(
            &format!("bs{bs}"),
            768 * MIB,
            &["-b", &size, "-L", "BLOCKS"],
            "BLOCKS",
            bs,
        );
    }
    let fresh = accepted(
        "ag8",
        2048 * MIB,
        &["-d", "agcount=8", "-L", "AGS"],
        "AGS",
        4096,
    );
    assert_eq!(db(&fresh, "agcount"), "8", "{fresh}");
}

#[test]
fn an_existing_filesystem_is_kept_unless_forced() {
    let volume = formatted("force", 512 * MIB, &["-L", "FIRST"]);
    let image = volume.path().to_str().unwrap();

    let again = tool("mkfs.xfs")
        .args(["-L", "SECOND", image])
        .output()
        .expect("spawn mkfs.xfs");
    assert!(
        !again.status.success(),
        "mkfs.xfs overwrote an existing filesystem without -f"
    );
    let label = stdout(&ok(tool("fs.xfs").args([image, "get", "label", "--text"])));
    assert_eq!(
        label.trim(),
        "FIRST",
        "the refused mkfs.xfs changed the volume"
    );

    ok(tool("mkfs.xfs").args(["-f", "-L", "SECOND", image]));
    let label = stdout(&ok(tool("fs.xfs").args([image, "get", "label", "--text"])));
    assert_eq!(
        label.trim(),
        "SECOND",
        "mkfs.xfs -f did not make a new filesystem"
    );
}

#[test]
fn a_device_too_small_is_refused_with_a_reason() {
    let volume =
        scratch::Volume::empty(SUITE, &format!("{}-tiny.img", std::process::id()), 8 * MIB);
    let out = tool("mkfs.xfs")
        .arg(volume.path())
        .output()
        .expect("spawn mkfs.xfs");
    assert!(!out.status.success(), "mkfs.xfs formatted an 8 MiB device");
    assert_ne!(
        out.status.code(),
        Some(101),
        "mkfs.xfs panicked: {}",
        stderr(&out)
    );
    assert!(
        stderr(&out).contains("too small"),
        "the refusal does not say the device is too small: {}",
        stderr(&out)
    );
}
