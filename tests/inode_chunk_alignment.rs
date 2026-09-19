//! A new inode chunk starts on the inode alignment, so the kernel finds
//! the inodes where the driver put them.
//!
//! With the align bit set and `sb_inoalignmt` at least a cluster, the
//! kernel maps an inode to its cluster buffer by masking its block down to
//! that alignment (`m_inoalign_mask` in `xfs_imap`), and `xfs_ialloc_ag_alloc`
//! allocates chunks only there. On 4 KiB blocks the alignment is 8 blocks;
//! on 1 KiB blocks it is 32.
//!
//! The driver took a new chunk's blocks from the first free run long enough,
//! wherever that run started. On 1 KiB blocks, after a few one-block
//! writes, free space started at block 91, so the chunk went there. The
//! kernel then replayed the record against the cluster at block 80, which
//! held file data, and refused the log: "metadata I/O error in
//! xlog_recover_items_pass2 … error 117".
//!
//! This fills the first chunk with files that each get one block of data,
//! which leaves free space starting off the alignment, then creates past
//! it. Every step is replayed by the kernel and checked by `xfs_repair -n`.
//! It skips when no kernel is reachable (see `common::transport`), and
//! ci-test.sh turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "inode_chunk_alignment";
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Mount, apply `f`, unmount; then have the kernel replay the record and
/// `xfs_repair -n` judge the result.
fn step(
    image: &str,
    name: &str,
    what: &str,
    f: impl FnOnce(&Filesystem) -> Result<(), fs_xfs::Error>,
) -> Option<()> {
    {
        let dev = Arc::new(FileDevice::open_rw(image).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>)
            .unwrap_or_else(|e| panic!("{what}: mount_rw after a replay: {e:?}"));
        f(&fs).unwrap_or_else(|e| panic!("{what}: {e:?}"));
    }
    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {name} "$m"; then
            umount "$m"
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ))?;
    assert!(
        out.contains("MOUNTED"),
        "after {what}, the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, &format!("after {what}"));
    Some(())
}

/// Inodes allocated across every group.
fn inodes(fs: &Filesystem) -> u32 {
    (0..fs.superblock().agcount)
        .map(|ag| fs.read_agi(ag).unwrap().count)
        .sum()
}

/// Removes the image however the test ends: every suite reads each `.img`
/// in the share as a fixture. And the share itself when this test made it,
/// because a suite that finds an empty share fails where a missing one
/// skips (`log_oracle` in the fixture-less test jobs).
#[test]
fn a_new_inode_chunk_on_one_kib_blocks_replays() {
    // NO FIXTURE DIRECTORY MEANS NO FIXTURE SET. This builds its own
    // volume, but it builds it in the share, and a share that exists is
    // what the suites scanning it take for a fixture set: creating one
    // here makes them fail where they would have skipped. The job that
    // runs this builds the fixtures first, so the directory is there.
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        320 * 1024 * 1024,
    );
    let image = scratch.path().to_path_buf();
    let name = scratch.guest();
    let Some(mkfs) = kernel_run(&format!(
        "mkfs.xfs -q -f -b size=1024 -d agcount=2 {name} 2>&1 && echo MKFS_OK; echo DONE"
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(mkfs.contains("MKFS_OK"), "mkfs.xfs failed:\n{mkfs}");
    let path = image.to_str().unwrap().to_string();

    let (inodes_before, align) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).unwrap())).unwrap();
        (inodes(&fs), fs.superblock().inoalignmt)
    };
    assert_eq!(
        align, 32,
        "mkfs.xfs gave 1 KiB blocks a different alignment"
    );

    // Short-form directories hold a handful of entries each, so the
    // files are spread over directories until the first chunk is full.
    let mut made = 0u32;
    'fill: for d in 0.. {
        let dir = format!("d{d}");
        step(&path, &name, &format!("mkdir /{dir}"), |fs| {
            let root = fs.lookup_path("/").unwrap().ino;
            fs.create_directory(root, dir.as_bytes(), 0o40755)
                .map(|_| ())
        })
        .expect("kernel");
        for f in 0..6 {
            let file = format!("/{dir}/f{f}");
            step(&path, &name, &format!("create {file}"), |fs| {
                let parent = fs.lookup_path(&format!("/{dir}")).unwrap().ino;
                fs.create_file(parent, format!("f{f}").as_bytes(), 0o100644)
                    .map(|_| ())
            })
            .expect("kernel");
            step(&path, &name, &format!("write {file}"), |fs| {
                let ino = fs.lookup_path(&file).unwrap().ino;
                fs.write_into_empty_file(ino, &[7u8; 1024]).map(|_| ())
            })
            .expect("kernel");
            made += 1;
            let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).unwrap())).unwrap();
            if inodes(&fs) > inodes_before {
                break 'fill;
            }
            assert!(made < 200, "no new inode chunk after {made} files");
        }
    }

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).unwrap())).unwrap();
    assert!(
        inodes(&fs) > inodes_before,
        "the test never needed a second chunk, so it checked nothing"
    );
}
