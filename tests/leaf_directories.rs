//! A directory grows past one block into leaf form, and shrinks back, as
//! names are added and removed (#366).
//!
//! A block-form directory holds its entries and their hash index in one
//! block. The next name after that is full moves it into leaf form: data
//! blocks of entries and a leaf block of index. Removing names packs it
//! back, and a directory whose entries fit one block again is block form
//! again.
//!
//! Each step is checked from the outside: every name lists, every name
//! looks up to its inode, nothing else lists, and the directory's shape
//! (one extent of one block, or data blocks and a leaf at the leaf
//! offset) is the one its size calls for. What the kernel and
//! `xfs_repair` make of the same directories is
//! `tests/leaf_directories_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

struct Sparse(Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]))[within..within + n]
                .copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn mounted() -> Filesystem {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn name(i: usize) -> Vec<u8> {
    format!("a-file-in-a-large-directory-{i:04}").into_bytes()
}

/// Every name the directory lists, each looked up to the inode it names.
fn listed(fs: &Filesystem, dir: u64) -> BTreeMap<Vec<u8>, u64> {
    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    let mut out = BTreeMap::new();
    for e in fs.read_dir(&inode, &raw).expect("readdir") {
        let found = fs.lookup(&inode, &raw, &e.name).expect("lookup").ino;
        assert_eq!(
            found, e.ino,
            "{:?} lists one inode and looks up another",
            e.name
        );
        assert!(
            out.insert(e.name.clone(), e.ino).is_none(),
            "{:?} listed twice",
            e.name
        );
    }
    out
}

/// Whether the directory is in leaf form: an extent at the leaf offset.
fn is_leaf_form(fs: &Filesystem, dir: u64) -> bool {
    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    let leaf = (1u64 << 35) / u64::from(fs.superblock().blocksize);
    fs.data_extents(&inode, &raw)
        .expect("extents")
        .iter()
        .any(|e| e.startoff == leaf)
}

#[test]
fn a_directory_grows_into_leaf_form_and_every_name_is_found() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    let mut want = BTreeMap::new();
    for i in 0..300 {
        let (ino, _) = fs
            .create_file(dir, &name(i), 0o100644)
            .unwrap_or_else(|e| panic!("create {i}: {e:?}"));
        want.insert(name(i), ino);
    }
    assert!(
        is_leaf_form(&fs, dir),
        "300 names did not move the directory into leaf form"
    );
    assert_eq!(listed(&fs, dir), want);
    let again = fs.create_file(dir, &name(17), 0o100644);
    assert!(
        matches!(again, Err(fs_xfs::Error::AlreadyExists)),
        "a name already in a leaf-form directory: {again:?}"
    );
    let inode = fs.read_inode(dir).expect("dir");
    let dirblock = u64::from(fs.superblock().dirblocksize());
    assert_eq!(
        inode.size % dirblock,
        0,
        "the size is not whole data blocks"
    );
    assert!(
        inode.size > dirblock,
        "a leaf-form directory of one data block"
    );
}

#[test]
fn subdirectories_in_a_leaf_form_directory_keep_their_links() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    for i in 0..150 {
        fs.create_directory(dir, &name(i), 0o040755)
            .expect("mkdir inside");
    }
    assert!(is_leaf_form(&fs, dir));
    assert_eq!(fs.read_inode(dir).expect("dir").nlink, 2 + 150);
    let names: BTreeSet<Vec<u8>> = listed(&fs, dir).into_keys().collect();
    assert_eq!(names, (0..150).map(name).collect());
}

#[test]
fn removing_names_packs_a_leaf_form_directory_back_into_one_block() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    let mut want = BTreeMap::new();
    for i in 0..300 {
        let (ino, _) = fs.create_file(dir, &name(i), 0o100644).expect("create");
        want.insert(name(i), ino);
    }
    let blocks_before = fs.read_inode(dir).expect("dir").nblocks;
    for i in 10..300 {
        fs.unlink_file(dir, &name(i))
            .unwrap_or_else(|e| panic!("unlink {i}: {e:?}"));
        want.remove(&name(i));
    }
    assert!(
        !is_leaf_form(&fs, dir),
        "ten names left the directory in leaf form"
    );
    assert_eq!(listed(&fs, dir), want);
    let inode = fs.read_inode(dir).expect("dir");
    assert_eq!(u64::from(fs.superblock().dirblocksize()), inode.size);
    assert!(inode.nblocks < blocks_before, "no block was given back");
}

#[test]
fn removing_some_names_keeps_leaf_form_with_fewer_data_blocks() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    let mut want = BTreeMap::new();
    for i in 0..300 {
        let (ino, _) = fs.create_file(dir, &name(i), 0o100644).expect("create");
        want.insert(name(i), ino);
    }
    let size_before = fs.read_inode(dir).expect("dir").size;
    for i in (0..300).step_by(3) {
        fs.unlink_file(dir, &name(i)).expect("unlink");
        want.remove(&name(i));
    }
    assert!(is_leaf_form(&fs, dir));
    assert_eq!(listed(&fs, dir), want);
    assert!(
        fs.read_inode(dir).expect("dir").size < size_before,
        "nothing was packed"
    );
    // A name removed can be made again, and lands somewhere findable.
    fs.create_file(dir, &name(0), 0o100644)
        .expect("create again");
    assert!(listed(&fs, dir).contains_key(&name(0)));
}

#[test]
fn a_name_is_removed_from_a_block_form_directory() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs
        .create_directory(root, b"middling", 0o040755)
        .expect("mkdir");
    for i in 0..40 {
        fs.create_file(dir, &name(i), 0o100644).expect("create");
    }
    assert!(!is_leaf_form(&fs, dir));
    fs.unlink_file(dir, &name(7))
        .expect("unlink from block form");
    let names: BTreeSet<Vec<u8>> = listed(&fs, dir).into_keys().collect();
    assert_eq!(names, (0..40).filter(|&i| i != 7).map(name).collect());
}

#[test]
fn a_subdirectory_is_removed_from_a_leaf_form_parent() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    for i in 0..150 {
        fs.create_directory(dir, &name(i), 0o040755)
            .expect("mkdir inside");
    }
    for i in 0..50 {
        fs.remove_directory(dir, &name(i)).expect("rmdir inside");
    }
    assert_eq!(fs.read_inode(dir).expect("dir").nlink, 2 + 100);
    let names: BTreeSet<Vec<u8>> = listed(&fs, dir).into_keys().collect();
    assert_eq!(names, (50..150).map(name).collect());
}

#[test]
fn a_name_is_renamed_in_leaf_and_block_form_directories() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    for (dir_name, count) in [(&b"leafy"[..], 300), (&b"blocky"[..], 40)] {
        let (dir, _) = fs
            .create_directory(root, dir_name, 0o040755)
            .expect("mkdir");
        let mut want = BTreeMap::new();
        for i in 0..count {
            let (ino, _) = fs.create_file(dir, &name(i), 0o100644).expect("create");
            want.insert(name(i), ino);
        }
        let longer = b"renamed-to-a-considerably-longer-name-than-it-had-before".to_vec();
        fs.rename_in_directory(dir, &name(5), &longer)
            .unwrap_or_else(|e| panic!("{dir_name:?}: rename: {e:?}"));
        let ino = want.remove(&name(5)).expect("was there");
        want.insert(longer.clone(), ino);
        assert_eq!(listed(&fs, dir), want, "{dir_name:?}");
        let taken = fs.rename_in_directory(dir, &longer, &name(6));
        assert!(
            matches!(taken, Err(fs_xfs::Error::AlreadyExists)),
            "{dir_name:?}: renaming over a name: {taken:?}"
        );
        let missing = fs.rename_in_directory(dir, b"not-there", b"anything");
        assert!(
            matches!(missing, Err(fs_xfs::Error::NotFound)),
            "{missing:?}"
        );
    }
}
