//! Files this driver truncates to any length read back in the Linux
//! kernel as exactly their kept bytes, and `xfs_repair` agrees about the
//! blocks it freed (#370).
//!
//! The kernel makes every file here on a `mkfs.xfs` volume:
//!
//! - `one`: 64 KiB in one extent, cut inside its third block;
//! - `frag`: forty 4 KiB pieces 8 KiB apart, so its extents are a B+tree,
//!   cut at a block boundary after twelve pieces, which leaves few enough
//!   to list in the inode and frees the tree;
//! - `big`: three hundred such pieces, cut inside its last block, which
//!   frees nothing and keeps its tree;
//! - `copy`: a reflinked copy of `src`, cut at a block boundary, which
//!   must leave `src` reading as it did.
//!
//! A cut inside a block `src` shares is refused, because zeroing it in
//! place would change `copy`. The kernel mounts the result, which replays
//! the records, and must read every file to a model of its kept bytes;
//! `xfs_repair -n` must call the volume clean.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "truncate_to_oracle";
const K: usize = 1024;

/// Pieces of 4 KiB, each filled with its own byte, every 8 KiB.
fn pieces(count: usize) -> Vec<u8> {
    let mut out = vec![0u8; (count - 1) * 8 * K + 4 * K];
    for i in 0..count {
        let fill = 0x41 + (i % 26) as u8;
        out[i * 8 * K..i * 8 * K + 4 * K].fill(fill);
    }
    out
}

#[test]
fn the_kernel_reads_files_truncated_to_any_length() {
    let volume = scratch::Volume::empty(SUITE, "truncate.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -m reflink=1 {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        xfs_io -f -c 'pwrite -q -S 0x31 0 64k' "$m/one"
        for i in $(seq 0 39); do
            xfs_io -f -c "pwrite -q -S $((0x41 + i % 26)) $((i * 8192)) 4096" "$m/frag"
        done
        for i in $(seq 0 299); do
            xfs_io -f -c "pwrite -q -S $((0x41 + i % 26)) $((i * 8192)) 4096" "$m/big"
        done
        xfs_io -f -c 'pwrite -q -S 0x51 0 64k' -c fsync "$m/src"
        cp --reflink=always "$m/src" "$m/copy" && echo COPY_OK
        sync
        xfs_io -c 'stat' "$m/big" | grep -q 'fsxattr.nextents = 300' && echo BIG_300
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK") && built.contains("COPY_OK"),
        "building the volume failed:\n{built}"
    );

    let mut want: Vec<(&str, Vec<u8>)> = Vec::new();
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = |p: &str| fs.lookup_path(p).expect(p).ino;
        let one = 10_000usize;
        fs.truncate_to(ino("/one"), one as u64)
            .expect("one: inside a block");
        want.push(("one", vec![0x31; one]));
        let frag = 12 * 8 * K;
        fs.truncate_to(ino("/frag"), frag as u64)
            .expect("frag: at a boundary, back to an inline list");
        want.push(("frag", pieces(40)[..frag].to_vec()));
        let big_full = pieces(300);
        let big = big_full.len() - 2000;
        fs.truncate_to(ino("/big"), big as u64)
            .expect("big: inside its last block, keeping its tree");
        want.push(("big", big_full[..big].to_vec()));
        fs.truncate_to(ino("/copy"), 32 * K as u64)
            .expect("copy: at a boundary");
        want.push(("copy", vec![0x51; 32 * K]));
        want.push(("src", vec![0x51; 64 * K]));
        let shared = fs.truncate_to(ino("/src"), 10_000);
        assert!(
            shared.is_err(),
            "a cut inside a shared block was not refused: {shared:?}"
        );
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            for f in one frag big copy src; do
                echo "FILE $f $(stat -c %s "$m/$f") $(sha256sum < "$m/$f" | cut -d' ' -f1)"
            done
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
    repair::assert_agreed(&out, "the volume after truncates to any length");
    for (name, bytes) in &want {
        let line = format!("FILE {name} {} {:x}", bytes.len(), Sha256::digest(bytes));
        assert!(
            out.lines().any(|l| l.trim() == line),
            "{name}: the kernel does not read the kept bytes.\nwant: {line}\n{out}"
        );
    }
}
