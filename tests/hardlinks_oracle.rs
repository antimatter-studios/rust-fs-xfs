//! Hard links this driver makes and removes are the ones the Linux kernel
//! reads, and a file's last name freeing it leaves nothing behind (#384).
//!
//! The kernel makes `a/kept` (12 KiB of 0x61) and `a/gone` (20 KiB of
//! 0x62) on a `mkfs.xfs` volume, and `big`, 300 names, so leaf form. The
//! driver then, on one mount: links `a/kept` as `b/kept2` and as
//! `big/kept3`, and unlinks `a/kept`; links `a/gone` as `b/gone2`, and
//! unlinks both names.
//!
//! The kernel mounts the result, which replays the records, and must find
//! `b/kept2` and `big/kept3` holding 12 KiB of 0x61 with two links, no
//! `a/kept`, and neither name of `gone`. `xfs_repair -n` must call the
//! volume clean, which is the check that `gone`'s inode and blocks went
//! back and nothing was left half-owned.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "hardlinks_oracle";

#[test]
fn the_kernel_reads_links_made_and_removed() {
    let volume = scratch::Volume::empty(SUITE, "links.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        mkdir "$m/a" "$m/b" "$m/big"
        xfs_io -f -c 'pwrite -q -S 0x61 0 12k' "$m/a/kept"
        xfs_io -f -c 'pwrite -q -S 0x62 0 20k' "$m/a/gone"
        for i in $(seq -w 0 299); do : > "$m/big/name-in-a-big-directory-$i"; done
        sync
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );

    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = |p: &str| fs.lookup_path(p).expect(p).ino;
        let (a, b, big) = (ino("/a"), ino("/b"), ino("/big"));
        let (kept, gone) = (ino("/a/kept"), ino("/a/gone"));
        fs.link(kept, b, b"kept2").expect("link kept into b");
        fs.link(kept, big, b"kept3").expect("link kept into big");
        fs.unlink_file(a, b"kept")
            .expect("unlink one of three names");
        fs.link(gone, b, b"gone2").expect("link gone into b");
        fs.unlink_file(a, b"gone").expect("unlink one of two names");
        fs.unlink_file(b, b"gone2").expect("unlink the last name");
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            ls -1 "$m/a" | sed 's|^|NAME a |'
            ls -1 "$m/b" | sed 's|^|NAME b |'
            for f in b/kept2 big/kept3; do
                echo "FILE $f $(stat -c '%h %s' "$m/$f") $(sha256sum < "$m/$f" | cut -d' ' -f1)"
            done
            echo "BIG $(ls -1 "$m/big" | wc -l)"
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
    repair::assert_agreed(&out, "the volume after links made and removed");
    let names = |dir: &str| -> Vec<&str> {
        out.lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("NAME {dir} ")))
            .collect()
    };
    assert!(names("a").is_empty(), "a still lists names:\n{out}");
    assert_eq!(names("b"), ["kept2"], "b:\n{out}");
    let sum = format!("{:x}", Sha256::digest(vec![0x61u8; 12 * 1024]));
    for f in ["b/kept2", "big/kept3"] {
        let want = format!("FILE {f} 2 12288 {sum}");
        assert!(
            out.lines().any(|l| l.trim() == want),
            "{f}: want {want}\n{out}"
        );
    }
    assert!(out.lines().any(|l| l.trim() == "BIG 301"), "big:\n{out}");
}
