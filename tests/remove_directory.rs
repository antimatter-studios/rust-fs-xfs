//! An empty directory is removed, and anything else is refused untouched
//! (#385).
//!
//! Removing a directory is an unlink with two more things to keep right:
//! the target's own `.` and `..` go with it, and its `..` was a link to
//! the parent, so the parent's link count falls by one. A directory that
//! still holds a name is refused with [`Error::DirectoryNotEmpty`], and a
//! name that is not a directory with [`Error::NotADirectory`], before
//! anything is written.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on a sparse in-memory device that counts writes.
//! What the kernel and `xfs_repair` make of a removal is
//! `tests/rmdir_replay_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: u64 = 512;

/// A device that keeps only the sectors written to it, and counts writes.
struct Sparse {
    sectors: Mutex<BTreeMap<u64, Vec<u8>>>,
    writes: AtomicU64,
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = sectors
                .get(&(at / SECTOR))
                .map_or(0, |s| s[(at % SECTOR) as usize]);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mut sectors = self.sectors.lock().unwrap();
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            sectors
                .entry(at / SECTOR)
                .or_insert_with(|| vec![0; SECTOR as usize])[(at % SECTOR) as usize] = byte;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn mounted() -> (Arc<Sparse>, Filesystem) {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
        writes: AtomicU64::new(0),
    });
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (dev, fs)
}

fn nlink(fs: &Filesystem, ino: u64) -> u32 {
    fs.read_inode(ino).expect("read inode").nlink
}

#[test]
fn an_empty_directory_is_removed_and_its_parent_loses_a_link() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let before = nlink(&fs, root);
    let (dir, _) = fs
        .create_directory(root, b"empty", 0o040755)
        .expect("mkdir");
    assert_eq!(
        nlink(&fs, root),
        before + 1,
        "mkdir should add the subdirectory's .."
    );

    let (removed, _) = fs.remove_directory(root, b"empty").expect("rmdir");
    assert_eq!(removed, dir);
    assert!(
        matches!(fs.lookup_path("/empty"), Err(Error::NotFound)),
        "the name is still there"
    );
    assert_eq!(
        nlink(&fs, root),
        before,
        "the parent kept the link the removed directory's .. held"
    );
    let freed = fs.read_inode(dir).expect("read the freed inode");
    assert_eq!(freed.mode, 0, "the removed directory's inode is not free");
    assert_eq!(freed.nlink, 0);
}

#[test]
fn the_freed_inode_is_found_again_by_a_create() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs
        .create_directory(root, b"empty", 0o040755)
        .expect("mkdir");
    fs.remove_directory(root, b"empty").expect("rmdir");
    let (again, _) = fs.create_file(root, b"next", 0o100644).expect("create");
    assert_eq!(
        again, dir,
        "the free-inode tree cannot find the inode rmdir gave back"
    );
}

#[test]
fn a_directory_that_holds_a_name_is_refused_and_nothing_is_written() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"full", 0o040755).expect("mkdir");
    fs.create_file(dir, b"inside", 0o100644)
        .expect("create inside");
    let writes = dev.writes.load(Ordering::SeqCst);
    let refused = fs.remove_directory(root, b"full");
    assert!(
        matches!(refused, Err(Error::DirectoryNotEmpty)),
        "a non-empty directory: {refused:?}"
    );
    assert_eq!(
        dev.writes.load(Ordering::SeqCst),
        writes,
        "the refusal wrote"
    );
    fs.lookup_path("/full/inside")
        .expect("the refusal changed the tree");
}

#[test]
fn a_name_that_is_not_a_directory_is_refused_and_nothing_is_written() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    fs.create_file(root, b"file", 0o100644).expect("create");
    let writes = dev.writes.load(Ordering::SeqCst);
    let refused = fs.remove_directory(root, b"file");
    assert!(
        matches!(refused, Err(Error::NotADirectory)),
        "a regular file: {refused:?}"
    );
    assert_eq!(
        dev.writes.load(Ordering::SeqCst),
        writes,
        "the refusal wrote"
    );
    let missing = fs.remove_directory(root, b"absent");
    assert!(
        matches!(missing, Err(Error::NotFound)),
        "a missing name: {missing:?}"
    );
}

#[test]
fn unlink_still_refuses_a_directory() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    assert!(
        fs.unlink_file(root, b"d").is_err(),
        "unlink removed a directory; that is remove_directory's job"
    );
}
