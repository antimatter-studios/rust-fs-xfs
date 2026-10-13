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
