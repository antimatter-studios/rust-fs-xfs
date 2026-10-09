//! Overwrites inside a file's written extents read back in the Linux
//! kernel as the bytes written, and change nothing else about the file
//! (#386).
//!
//! `write_at` overwrites bytes that already exist, in place, and touches no
//! metadata: no block is allocated or freed, the size does not move, and
//! the extent map stays exactly as it was. The kernel makes the file here,
//! four extents of 16 KiB with gaps between, so the writes meet extent
//! boundaries the kernel chose, and records its extent map and size. The
//! driver then overwrites:
//!
//! - exactly one block, aligned at both ends;
//! - the first and the last byte of an extent;
//! - a range across a block boundary, unaligned at both ends;
//! - one range five times over, each time with different bytes.
//!
//! A range across the gap between two extents runs through a hole, which
//! an in-place write cannot fill, and is refused rather than written.
//!
//! The kernel then reads the file to a model of every write, and its
//! extent map and size must be what they were; `xfs_repair -n` must call
//! the volume clean.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "overwrite_ranges_oracle";
const K: usize = 1024;

#[test]
fn overwrites_read_back_and_leave_the_map_alone() {
    let volume = scratch::Volume::empty(SUITE, "overwrite.img", 300 * 1024 * 1024);
    let image = volume.guest();
    // Four 16 KiB extents at 0, 32, 64 and 96 KiB, each its own byte.
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        for i in 0 1 2 3; do
            xfs_io -f -c "pwrite -q -S $((0x61 + i)) $((i * 32768)) 16384" -c fsync "$m/f"
        done
        echo "MAP $(xfs_bmap "$m/f" | tail -n +2 | tr -s ' ' | tr '\n' ';')"
        echo "SIZE $(stat -c %s "$m/f")"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let field = |out: &str, key: &str| -> String {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key}:\n{out}"))
            .to_string()
    };
    let map_before = field(&built, "MAP");
    let size_before = field(&built, "SIZE");

    let mut model = vec![0u8; 96 * K + 16 * K];
    for i in 0..4 {
        model[i * 32 * K..i * 32 * K + 16 * K].fill(0x61 + i as u8);
    }
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let bs = fs.superblock().blocksize as usize;
        let mut write = |at: usize, data: &[u8]| {
            let (inode, raw) = {
                let ino = fs.lookup_path("/f").expect("f").ino;
                fs.read_inode_raw(ino).expect("inode")
            };
            let n = fs
                .write_at(&inode, &raw, at as u64, data)
                .unwrap_or_else(|e| panic!("write {} at {at}: {e:?}", data.len()));
            assert_eq!(n, data.len());
            model[at..at + data.len()].copy_from_slice(data);
        };
        write(bs, &vec![0x01; bs]);
        write(32 * K, &[0x02]);
        write(48 * K - 1, &[0x03]);
        write(64 * K + bs - 7, &[0x04; 19]);
        for (round, fill) in [0x10u8, 0x11, 0x12, 0x13, 0x14].into_iter().enumerate() {
            let data: Vec<u8> = (0..333).map(|i| fill ^ (i as u8) ^ round as u8).collect();
            write(100 * K + 5, &data);
        }
    }
    // A range across the gap between two extents is not one in-place
    // write: the gap is a hole. It is two writes, one in each extent.
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = fs.lookup_path("/f").expect("f").ino;
        let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
        let across = fs.write_at(&inode, &raw, (16 * K - 10) as u64, &[0x55; 20]);
        assert!(
            across.is_err(),
            "a write into a hole was taken in place: {across:?}"
        );
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "MAP $(xfs_bmap "$m/f" | tail -n +2 | tr -s ' ' | tr '\n' ';')"
            echo "SIZE $(stat -c %s "$m/f")"
            echo "SHA $(sha256sum < "$m/f" | cut -d' ' -f1)"
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
    repair::assert_agreed(&out, "the volume after overwrites in place");
    assert_eq!(
        field(&out, "MAP"),
        map_before,
        "the extent map changed:\n{out}"
    );
    assert_eq!(field(&out, "SIZE"), size_before, "the size changed:\n{out}");
    assert_eq!(
        field(&out, "SHA"),
        format!("{:x}", Sha256::digest(&model)),
        "the kernel does not read the overwritten bytes"
    );
}
