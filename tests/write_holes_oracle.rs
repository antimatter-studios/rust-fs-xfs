//! Writes into holes, unwritten extents and past the end read back in the
//! Linux kernel as the bytes written and zeros everywhere else (#387).
//!
//! The kernel and `xfs_io` make every file here, so the holes, the
//! unwritten extents and the extents around them are the kernel's own:
//!
//! - `sparse`: 4 KiB at 0 and 4 KiB at 64 KiB, a hole between;
//! - `prealloc`: 64 KiB fallocated, one unwritten extent;
//! - `small`: 1000 bytes, its last block mostly past the end;
//! - `leading`: 4 KiB at 64 KiB, a hole before it.
//!
//! Before they are made, the free space is dirtied: a file of `0xEE` is
//! written and removed, so a block taken without being zero-filled reads
//! back as `0xEE` rather than as the zeros a fresh device would hide it
//! behind.
//!
//! The driver then writes into each case on one mount: inside the hole,
//! inside the unwritten extent, past the end of `small` with a gap, into
//! the leading hole, and past the end of `sparse`. The kernel mounts the
//! result, which replays the record, and every file must hash to a model
//! of what it should hold; `xfs_repair -n` must call the volume clean.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::sync::Arc;

const SUITE: &str = "write_holes_oracle";

const KIB: usize = 1024;

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(29).wrapping_add(seed) | 1)
        .collect()
}

/// A file's expected contents: a model the writes are applied to.
struct Model(Vec<u8>);

impl Model {
    fn write(&mut self, offset: usize, data: &[u8]) {
        if self.0.len() < offset + data.len() {
            self.0.resize(offset + data.len(), 0);
        }
        self.0[offset..offset + data.len()].copy_from_slice(data);
    }
}

#[test]
fn the_kernel_reads_writes_into_holes_and_unwritten_extents() {
    let volume = scratch::Volume::empty(SUITE, "holes.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        xfs_io -f -c 'pwrite -q -S 0xee 0 4m' -c fsync "$m/stale" && rm "$m/stale" && sync
        xfs_io -f -c 'pwrite -q -S 0x11 0 4k' -c 'pwrite -q -S 0x22 64k 4k' "$m/sparse"
        xfs_io -f -c 'falloc 0 64k' "$m/prealloc"
        xfs_io -f -c 'pwrite -q -S 0x33 0 1000' "$m/small"
        xfs_io -f -c 'pwrite -q -S 0x44 64k 4k' "$m/leading"
        sync
        xfs_io -c 'fiemap -v' "$m/prealloc" | grep -Eq ' 0x8[0-9a-f]{{2}}$' && echo PREALLOC_UNWRITTEN
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    assert!(
        built.contains("PREALLOC_UNWRITTEN"),
        "the fallocated file has no unwritten extent, so the case would not test one:\n{built}"
    );

    let mut sparse = Model(Vec::new());
    sparse.write(0, &[0x11; 4 * KIB]);
    sparse.write(64 * KIB, &[0x22; 4 * KIB]);
    let mut prealloc = Model(vec![0; 64 * KIB]);
    let mut small = Model(vec![0x33; 1000]);
    let mut leading = Model(Vec::new());
    leading.write(64 * KIB, &[0x44; 4 * KIB]);

    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = |name: &str| fs.lookup_path(name).expect(name).ino;
        let writes: [(&str, &mut Model, usize, Vec<u8>); 5] = [
            ("/sparse", &mut sparse, 20 * KIB + 7, pattern(9 * KIB, 1)),
            (
                "/prealloc",
                &mut prealloc,
                10 * KIB + 5,
                pattern(5 * KIB, 2),
            ),
            ("/small", &mut small, 10_000, pattern(3000, 3)),
            ("/leading", &mut leading, 100, pattern(2 * KIB, 4)),
            (
                "/sparse",
                &mut Model(Vec::new()),
                100 * KIB,
                pattern(KIB, 5),
            ),
        ];
        for (name, model, at, data) in writes {
            fs.write(ino(name), at as u64, &data)
                .unwrap_or_else(|e| panic!("{name}: write {} at {at}: {e:?}", data.len()));
            model.write(at, &data);
        }
    }
    sparse.write(100 * KIB, &pattern(KIB, 5));

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            for f in sparse prealloc small leading; do
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
    repair::assert_agreed(
        &out,
        "the volume after writes into holes and unwritten extents",
    );
    for (name, model) in [
        ("sparse", &sparse),
        ("prealloc", &prealloc),
        ("small", &small),
        ("leading", &leading),
    ] {
        let want = format!("FILE {name} {} {}", model.0.len(), sha256_hex(&model.0));
        assert!(
            out.lines().any(|l| l.trim() == want),
            "{name}: the kernel does not read what was written.\nwant: {want}\n{out}"
        );
    }
}
