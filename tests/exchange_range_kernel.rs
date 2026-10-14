//! An exchange this driver makes is one the Linux kernel replays and
//! reads, and `xfs_repair -n` accepts (#374).
//!
//! RUN ON THE DEBIAN 13 GUEST, `chore test:kernel-new`. Debian 12's 6.1
//! kernel refuses to mount a volume with the exchange-range bit, so it can
//! neither replay this driver's log nor read the files after it, and
//! `xfs_repair` will not grade a log nothing has replayed. Run on that
//! guest this fails at the mount, saying so, rather than passing.
//!
//! The guest's own `mkfs.xfs` (6.13) makes the volume with
//! `-i exchange=1` and its defaults, which include a reverse-mapping tree
//! and reflink, so the reverse map has to follow every moved block. The
//! kernel writes two files, `a` whole and `b` with a hole in its middle;
//! the driver exchanges their first 64 KiB; the kernel mounts a copy,
//! which replays the record, and reads both files, and `xfs_repair -n`
//! walks the result.

mod common;

use common::{guest_quote, kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::inode::Format;
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "exchange_range_kernel";
const KIB: usize = 1024;

fn sha(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[test]
fn an_exchange_the_driver_logs_is_replayed_and_read_by_the_kernel() {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-exchange.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let built = kernel_run(&format!(
        r#"
        set -e
        mkfs.xfs -q -f -i exchange=1 {image}
        m=$(mktemp -d)
        mount -o loop {image} "$m"
        xfs_io -f -c 'pwrite -q -S 0x11 0 64k' -c fsync "$m/a"
        xfs_io -f -c 'pwrite -q -S 0x22 0 16k' -c 'pwrite -q -S 0x22 48k 16k' -c fsync "$m/b"
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(!built.contains("UMOUNT_FAILED"), "{built}");

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        assert!(
            fs.superblock().has_rmapbt(),
            "the guest's mkfs made no reverse map, so nothing here grades one"
        );
        let a = fs.lookup_path("/a").expect("a").ino;
        let b = fs.lookup_path("/b").expect("b").ino;
        fs.exchange_range(a, 0, b, 0, 64 * KIB as u64)
            .expect("exchange");
    }

    // What each should read: a takes b's blocks and its hole, b takes a's.
    let mut want_a = vec![0x22u8; 64 * KIB];
    want_a[16 * KIB..48 * KIB].fill(0);
    let want_b = vec![0x11u8; 64 * KIB];
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/exchange-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "SUM a $(sha256sum < "$m/a" | cut -d' ' -f1)"
            echo "SUM b $(sha256sum < "$m/b" | cut -d' ' -f1)"
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        rmdir "$m"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        repair = repair::script("\"$img\""),
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume, which on a 6.1 kernel is the exchange-range bit \
         itself: run this on the Debian 13 guest (chore test:kernel-new):\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    let sum = |name: &str| {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("SUM {name} ")))
            .unwrap_or_else(|| panic!("no checksum for {name}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(
        sum("a"),
        sha(&want_a),
        "a reads other than b's blocks and hole"
    );
    assert_eq!(sum("b"), sha(&want_b), "b reads other than a's blocks");
    repair::assert_agreed(
        &out,
        "the volume after the driver's exchange, replayed by the kernel",
    );
}

/// Make a volume with the guest's `mkfs.xfs -i exchange=1`, run `script`
/// in it as the kernel mounted it, and unmount.
fn kernel_made(suite_file: &str, script: &str) -> scratch::Volume {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-{suite_file}", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let built = kernel_run(&format!(
        r#"
        set -e
        mkfs.xfs -q -f -i exchange=1 {image}
        m=$(mktemp -d)
        mount -o loop {image} "$m"
        cd "$m"
        {script}
        cd /
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("DONE") && !built.contains("UMOUNT_FAILED"),
        "{built}"
    );
    volume
}

/// The kernel replays a copy of `volume`, prints each file's checksum,
/// and `xfs_repair -n` walks the result; returns what it printed, with
/// every file's checksum checked against `want`.
fn replayed_and_graded(volume: &scratch::Volume, want: &[(&str, &[u8])], what: &str) {
    let image = guest_quote(&volume.guest());
    let mut sums = String::new();
    for (name, _) in want {
        sums.push_str(&format!(
            "echo \"SUM {name} $(sha256sum < \"$m/{name}\" | cut -d' ' -f1)\"\n"
        ));
    }
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/exchange-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {sums}
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        rmdir "$m"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        repair = repair::script("\"$img\""),
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "{what}: the kernel refused the volume:\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{what}: {out}");
    for (name, bytes) in want {
        let got = out
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("SUM {name} ")))
            .unwrap_or_else(|| panic!("{what}: no checksum for {name}:\n{out}"));
        assert_eq!(
            got.trim(),
            sha(bytes),
            "{what}: {name} reads other than it should"
        );
    }
    repair::assert_agreed(&out, what);
}

/// A reflinked file's blocks move without their reference counts changing
/// (#441). The kernel writes `a`, reflinks it to `c`, and writes `b`; the
/// driver exchanges all of `a` with all of `b`. Now `b` and `c` share the
/// blocks `a` and `c` shared, `b` carries the reflink flag, and
/// `xfs_repair -n` checks the reference counts, the overlapping
/// reverse-map records and the flags against the files' maps.
#[test]
fn an_exchange_moves_reflinked_blocks_and_their_owners_follow() {
    let volume = kernel_made(
        "reflink.img",
        r#"
        xfs_io -f -c 'pwrite -q -S 0x11 0 64k' -c fsync a
        cp --reflink=always a c
        xfs_io -f -c 'pwrite -q -S 0x22 0 64k' -c fsync b
        "#,
    );
    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        assert!(
            fs.superblock().has_reflink(),
            "the guest's mkfs made no reference-count tree, so nothing here is shared"
        );
        let a = fs.lookup_path("/a").expect("a");
        let b = fs.lookup_path("/b").expect("b");
        assert!(
            a.has_shared_extents() && !b.has_shared_extents(),
            "the kernel's reflink did not leave a shared and b not"
        );
        fs.exchange_range(a.ino, 0, b.ino, 0, 64 * KIB as u64)
            .expect("exchange");
        assert!(
            fs.lookup_path("/b").expect("b").has_shared_extents(),
            "b took shared blocks without the reflink flag"
        );
    }
    let ones = vec![0x11u8; 64 * KIB];
    let twos = vec![0x22u8; 64 * KIB];
    replayed_and_graded(
        &volume,
        &[("a", &twos), ("b", &ones), ("c", &ones)],
        "the volume after exchanging a reflinked file's blocks",
    );
}

/// An exchange edits a block-map B+tree, and lays a list out in whichever
/// form its new length needs (#441). The kernel writes `f` as 60 blocks
/// with a hole after each, more extents than an inode lists, so a tree;
/// and `g` as 120 contiguous blocks. Two exchanges then move every
/// fragment of `f` into `g`: `g` becomes a tree and `f`, contiguous
/// again, an inline list.
#[test]
fn an_exchange_rebuilds_block_map_trees_both_ways() {
    let volume = kernel_made(
        "btree.img",
        r#"
        for i in $(seq 59 -1 0); do
            xfs_io -f -c "pwrite -q -S 0x33 $((i * 8))k 4k" -c fsync f
        done
        xfs_io -f -c 'pwrite -q -S 0x44 0 480k' -c fsync g
        "#,
    );
    let half = 240 * KIB as u64;
    let rest = 472 * KIB as u64 - half;
    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let f = fs.lookup_path("/f").expect("f");
        let g = fs.lookup_path("/g").expect("g");
        assert_eq!(
            f.format,
            Format::Btree,
            "the kernel listed f's extents inline"
        );
        assert_eq!(g.format, Format::Extents);
        fs.exchange_range(f.ino, 0, g.ino, 0, half)
            .expect("the first half");
        fs.exchange_range(f.ino, half, g.ino, half, rest)
            .expect("the rest");
        assert_eq!(
            fs.lookup_path("/f").expect("f").format,
            Format::Extents,
            "f fits its inode again"
        );
        assert_eq!(
            fs.lookup_path("/g").expect("g").format,
            Format::Btree,
            "g holds every fragment"
        );
    }
    let mut fragments = vec![0u8; 476 * KIB];
    for i in 0..60 {
        fragments[i * 8 * KIB..i * 8 * KIB + 4 * KIB].fill(0x33);
    }
    let mut want_f = vec![0x44u8; 472 * KIB];
    want_f.extend_from_slice(&fragments[472 * KIB..]);
    let mut want_g = fragments[..472 * KIB].to_vec();
    want_g.extend_from_slice(&[0x44u8; 8 * KIB]);
    replayed_and_graded(
        &volume,
        &[("f", &want_f), ("g", &want_g)],
        "the volume after exchanges through block-map trees",
    );
}
