//! What the reverse-mapping tree says after this driver has written.
//!
//! # Why this exists separately from the feature matrix
//!
//! The matrix asks `xfs_repair` whether the filesystem is sound, and
//! that is the right question for most things. It does not settle this
//! one. Measured, by deleting the removal step and running the matrix
//! again: an allocation with no record makes `xfs_repair` fail —
//!
//! ```text
//! Missing reverse-mapping record for (0/13) len 1 owner 131 off 0
//! ```
//!
//! — but a free that leaves its record behind does not. The checker is
//! content, the matrix reported every row sound, and the tree was
//! nonetheless describing an extent that had gone back to free space.
//!
//! A test that cannot fail when the thing it covers is removed is not
//! covering it. So this asks the tree directly: after the kernel has
//! replayed what this driver logged, is the record there or not?

mod common;
use common::{kernel_run, share};

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let dir = share().join("scratch");
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A filesystem with a reverse-mapping tree and something in it.
fn fixture() -> Option<PathBuf> {
    let p = share().join("xfsfeat-rmapbt.img");
    p.exists().then_some(p)
}

/// Have the kernel replay the log, so the image reflects what was
/// logged rather than what was on disk before it.
fn replay(img: &Path) -> bool {
    let name = img.file_name().unwrap().to_string_lossy().into_owned();
    let script = format!(
        r#"
        m=$(mktemp -d)
        mount -o loop,nouuid /share/scratch/{name} "$m" && umount "$m" || echo MOUNT_FAILED
        rmdir "$m" 2>/dev/null
        echo DONE
        "#
    );
    match kernel_run(&script) {
        Some(out) => !out.contains("MOUNT_FAILED"),
        None => false,
    }
}

fn records_owned_by(img: &Path, owner: i64) -> Vec<fs_xfs::rmap::Rmap> {
    let dev = FileDevice::open(img).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    fs.rmap_records(0)
        .expect("read the reverse-mapping tree")
        .into_iter()
        .filter(|r| r.owner == owner)
        .collect()
}

/// Freeing a file's blocks takes its records out of the tree.
///
/// THIS IS THE ONE `xfs_repair` DOES NOT CATCH. A record left behind
/// describes an extent that is now free, and the checker says nothing —
/// so if this driver skipped the removal, every other test in the
/// repository would still pass.
#[test]
fn freeing_a_file_removes_its_reverse_mapping_records() {
    let Some(source) = fixture() else {
        eprintln!("no xfsfeat-rmapbt fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "rmap-free-scratch.img");
    let img = scratch.path();

    let ino = {
        let dev = FileDevice::open(img).expect("open");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        fs.lookup_path("/sf/data.bin").expect("the file").ino
    };

    let before = records_owned_by(img, ino as i64);
    assert!(
        !before.is_empty(),
        "the fixture's file must own blocks, or there is nothing to free"
    );
    let blocks: u32 = before.iter().map(|r| r.blockcount).sum();

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        fs.truncate_to_zero(ino).expect("truncate");
    }

    if !replay(img) {
        eprintln!("no kernel to replay the record — skipping the check");
        return;
    }

    let after = records_owned_by(img, ino as i64);
    assert!(
        after.is_empty(),
        "inode {ino} gave up {blocks} blocks and the tree still says it owns {}: {after:?}",
        after.iter().map(|r| r.blockcount).sum::<u32>()
    );
}

/// Allocating blocks for a file puts a record in, saying which inode
/// owns them and where in the file they sit.
#[test]
fn writing_a_file_adds_a_reverse_mapping_record_for_it() {
    let Some(source) = fixture() else {
        eprintln!("no xfsfeat-rmapbt fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "rmap-alloc-scratch.img");
    let img = scratch.path();

    let ino = {
        let dev = FileDevice::open(img).expect("open");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        fs.lookup_path("/sf/empty.bin").expect("the empty file").ino
    };
    assert!(
        records_owned_by(img, ino as i64).is_empty(),
        "an empty file owns nothing yet, or this proves nothing"
    );

    let data = vec![0xABu8; 8192];
    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        fs.write_into_empty_file(ino, &data).expect("write");
    }

    if !replay(img) {
        eprintln!("no kernel to replay the record — skipping the check");
        return;
    }

    let after = records_owned_by(img, ino as i64);
    assert_eq!(
        after.len(),
        1,
        "one contiguous run should have produced exactly one record, got {after:?}"
    );
    assert_eq!(
        after[0].file_offset(),
        0,
        "the file was empty, so its blocks start at offset zero"
    );
    assert_eq!(
        after[0].flags(),
        0,
        "written data in the data fork carries none of the three flags"
    );

    // And the tree agrees with the inode about how much it holds.
    let dev = FileDevice::open(img).expect("open");
    let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
    let (inode, raw) = fs.read_inode_raw(ino).expect("read the inode back");
    let mapped: u64 = fs
        .data_extents(&inode, &raw)
        .expect("its extents")
        .iter()
        .map(|e| e.blockcount)
        .sum();
    assert_eq!(
        u64::from(after[0].blockcount),
        mapped,
        "the tree and the inode must agree about how many blocks the file holds"
    );
}
