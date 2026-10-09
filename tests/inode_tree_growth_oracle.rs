//! A group's inode trees, grown by this driver past one block, are the
//! trees the Linux kernel reads and grows further, and `xfs_repair`
//! accepts (#423).
//!
//! `mkfs.xfs -b size=1024` makes the volume, so an inode-tree block holds
//! 60 chunk records and the trees outgrow one block at the 61st chunk of
//! 64 inodes. The driver creates nine thousand files, which takes the
//! inode tree to three leaves under a root, and the free-inode tree with
//! it. The kernel mounts the result, counts and reads the files, and
//! makes a thousand more of its own in the trees the driver laid out.
//!
//! `xfs_repair -n` must call the volume clean: every record of both
//! trees, `agi_level`, the block counts `inobtcount` keeps in the AGI,
//! and the free space the trees' blocks came out of. A second volume has
//! the reverse-mapping tree, where every block of the inode trees must
//! be owned by `OWN_INOBT`, as the kernel's own allocations are.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

const SUITE: &str = "inode_tree_growth_oracle";
const FILES: usize = 9000;

fn grow_and_check(img: &str, mkfs_options: &str) {
    let volume = scratch::Volume::empty(SUITE, img, 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -b size=1024 {mkfs_options} {image} 2>&1 && echo MKFS_OK
        echo DONE
        "#
    ));
    assert!(built.contains("MKFS_OK"), "mkfs.xfs failed:\n{built}");

    let level = {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        for d in 0..FILES.div_ceil(100) {
            let (dir, _) = fs
                .create_directory(root, format!("d{d}").as_bytes(), 0o040755)
                .unwrap_or_else(|e| panic!("mkdir {d}: {e:?}"));
            for i in 0..100.min(FILES - d * 100) {
                fs.create_file(dir, format!("f{i}").as_bytes(), 0o100644)
                    .unwrap_or_else(|e| panic!("dir {d} file {i}: {e:?}"));
            }
        }
        // The group's AGI, as this driver now reads it: the tree's depth.
        let mut agi = vec![0u8; 512];
        fs.device()
            .read_at(2 * u64::from(fs.superblock().sectsize), &mut agi)
            .expect("agi");
        u32::from_be_bytes(agi[24..28].try_into().unwrap())
    };
    assert!(
        level >= 2,
        "{FILES} files left the inode tree {level} level deep"
    );

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "FILES $(find "$m" -type f | wc -l)"
            echo "READ $(stat -c %s "$m/d0/f0" "$m/d89/f99" | tr '\n' ' ')"
            mkdir "$m/k"
            (cd "$m/k" && seq -f 'k%g' 1 1000 | xargs touch) && echo KERNEL_MADE
            echo "AFTER $(find "$m" -type f | wc -l)"
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "LEVEL $(xfs_db -r -c 'agi 0' -c 'print level' "$img")"
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
        &format!("inode trees grown past one block ({mkfs_options})"),
    );
    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("no {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(
        field("FILES"),
        FILES.to_string(),
        "the kernel counts other files"
    );
    assert_eq!(field("READ"), "0 0", "the kernel reads the files");
    assert!(
        out.contains("KERNEL_MADE"),
        "the kernel could not create in the trees:\n{out}"
    );
    assert_eq!(field("AFTER"), (FILES + 1000).to_string());
    assert!(
        field("LEVEL").contains(&format!("level = {level}")),
        "xfs_db reads another depth than {level}:\n{out}"
    );
}

#[test]
fn inode_trees_grown_past_one_block_are_the_kernels() {
    grow_and_check("inobt.img", "");
}

#[test]
fn inode_tree_blocks_are_owned_by_the_inode_trees_in_the_reverse_map() {
    grow_and_check("inobt-rmap.img", "-m rmapbt=1");
}
