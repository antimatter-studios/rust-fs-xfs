//! A file whose extents live in a B+tree truncates to zero (#222).
//!
//! Once a file has more extents than its inode can hold, the map moves into
//! a B+tree of its own. Reading such a file already worked; freeing it was
//! refused, because the tree's own blocks belong to the inode and have to go
//! back with the data. Any file large or fragmented enough is this shape, so
//! what was refused is most of them.
//!
//! The kernel writes the file in pieces, this driver truncates it, and the
//! kernel replays the record. What the volume holds afterwards is the
//! judgement: the file empty, `xfs_repair` content, and the free space back
//! to what it was before the file existed — which is what says the tree's
//! blocks went back too, not only the data.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "truncate_btree_fork";
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// One-block pieces, each its own extent. Enough that the map cannot stay
/// in the inode: a v3 inode holds a few dozen records at most.
const PIECES: u32 = 3000;

#[test]
fn a_file_with_a_btree_fork_is_truncated() {
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let image = scratch.path().to_path_buf();
    let name = scratch.guest();

    // Built, measured empty, then filled: the free-block count before the
    // file existed is what the truncate has to give back.
    let Some(built) = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -m rmapbt=1 {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        echo "FREE_BEFORE $(df --output=avail -k "$m" | tail -1)"
        # Every other block, so no two pieces are adjacent and each is a
        # record of its own.
        for i in $(seq 0 {last}); do
            dd if=/dev/zero of="$m/frag" bs=4096 count=1 seek=$((i * 2)) conv=notrunc status=none
        done
        sync
        echo "EXTENTS $(xfs_bmap "$m/frag" | grep -c ':')"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#,
        last = PIECES - 1
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let guest = |out: &str, key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    let free_before = guest(&built, "FREE_BEFORE");
    assert!(
        guest(&built, "EXTENTS").parse::<u32>().unwrap() > 100,
        "the file is not fragmented enough to have left the inode"
    );

    let path = image.to_str().unwrap().to_string();
    {
        let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = fs.lookup_path("/frag").expect("the fragmented file").ino;
        let (inode, _) = fs.read_inode_raw(ino).expect("its inode");
        assert_eq!(
            inode.format,
            fs_xfs::inode::Format::Btree,
            "the file's map is not a B+tree, so this tests nothing"
        );
        fs.truncate_to_zero(ino).expect("truncate a B+tree fork");
    }

    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {name} "$m"; then
            echo "SIZE $(stat -c %s "$m/frag")"
            echo "BLOCKS $(stat -c %b "$m/frag")"
            echo "FREE_AFTER $(df --output=avail -k "$m" | tail -1)"
            umount "$m" || echo UMOUNT_FAILED
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ))
    .expect("kernel");

    assert!(
        out.contains("MOUNTED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, "the volume after the truncate");
    assert_eq!(guest(&out, "SIZE"), "0", "the file after the truncate");
    assert_eq!(guest(&out, "BLOCKS"), "0", "and what it still holds");

    // THE TREE'S OWN BLOCKS WENT BACK TOO. The data alone would leave the
    // map's blocks allocated and counted against the volume, which is the
    // leak this refusal existed to avoid.
    let (before, after) = (
        free_before.parse::<i64>().unwrap(),
        guest(&out, "FREE_AFTER").parse::<i64>().unwrap(),
    );
    assert!(
        (before - after).abs() <= 64,
        "the volume had {before} KiB free before the file and {after} KiB after \
         truncating it, so something it held was not given back"
    );
}
