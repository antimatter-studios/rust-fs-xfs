//! A file this driver places across allocation groups is the file the
//! Linux kernel reads (#388).
//!
//! `mkfs.xfs -d agcount=8` in the harness guest makes groups of about
//! 37 MiB, and the driver writes one 50 MiB file: more than the inode's
//! own group can hold, so it is placed across groups. The kernel then
//! mounts the volume, which replays the record, and must read the file to
//! the hash of what was written; `xfs_bmap -v` must show it in more than
//! one group, and `xfs_repair -n` must call the volume clean, which is the
//! check on every group's free-space trees and counters.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "write_across_groups_oracle";
const FILE_BYTES: usize = 50 * 1024 * 1024;

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i / 4096) as u8) ^ (i as u8).wrapping_mul(13) | 1)
        .collect()
}

#[test]
fn the_kernel_reads_a_file_placed_across_groups() {
    let volume = scratch::Volume::empty(SUITE, "groups.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -d agcount=8 {image} 2>&1 && echo MKFS_OK
        echo DONE
        "#
    ));
    assert!(built.contains("MKFS_OK"), "mkfs.xfs failed:\n{built}");

    let data = pattern(FILE_BYTES);
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        let (ino, _) = fs.create_file(root, b"big", 0o100644).expect("create");
        fs.write(ino, 0, &data).expect("write 50 MiB");
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "FILE $(stat -c %s "$m/big") $(sha256sum < "$m/big" | cut -d' ' -f1)"
            xfs_bmap -v "$m/big" | awk 'NR > 2 && $4 ~ /^[0-9]+$/ {{ print "AG " $4 }}' | sort -u
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
    repair::assert_agreed(&out, "the volume holding a file placed across groups");
    let want = format!("FILE {} {:x}", data.len(), Sha256::digest(&data));
    assert!(
        out.lines().any(|l| l.trim() == want),
        "the kernel does not read the file as written.\nwant: {want}\n{out}"
    );
    let groups = out.lines().filter(|l| l.trim().starts_with("AG ")).count();
    assert!(
        groups >= 2,
        "xfs_bmap shows the file in {groups} group(s), so nothing was placed across groups:\n{out}"
    );
}
