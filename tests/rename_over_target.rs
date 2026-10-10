//! A rename replaces a name that is already there, freeing what it named,
//! and refuses what POSIX forbids without writing (#383).
//!
//! A file replaces a file and a directory replaces an empty directory.
//! The replaced inode, left with no link, is freed in the same record: its
//! slot goes back to its chunk, so the next create finds it, and its
//! blocks go back to free space. A file over a directory, a directory over
//! a file, and a directory over one that is not empty are refused, each
//! with its own error, before anything is written.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device that counts writes. A
//! replaced file with another link, which needs the kernel to make, and
//! the kernel's reading of every case, are
//! `tests/rename_over_target_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

struct Sparse {
    sectors: Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>,
    writes: AtomicU64,
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
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
        self.writes.fetch_add(1, Ordering::SeqCst);
        let mut sectors = self.sectors.lock().unwrap();
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

fn mounted() -> (Arc<Sparse>, Filesystem) {
    let dev = Arc::new(Sparse {
        sectors: Mutex::new(BTreeMap::new()),
        writes: AtomicU64::new(0),
    });
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (dev, fs)
}

fn names(fs: &Filesystem, dir: u64) -> BTreeMap<Vec<u8>, u64> {
    let (inode, raw) = fs.read_inode_raw(dir).expect("dir");
    fs.read_dir(&inode, &raw)
        .expect("readdir")
        .into_iter()
        .map(|e| (e.name, e.ino))
        .collect()
}

fn is_free(fs: &Filesystem, ino: u64) -> bool {
    let inode = fs.read_inode(ino).expect("inode");
    inode.mode == 0 && inode.nlink == 0
}

#[test]
fn a_file_replaces_a_file_and_the_replaced_one_is_freed() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir");
    let (moved, _) = fs.create_file(a, b"new", 0o100644).expect("create");
    let (old, _) = fs.create_file(b, b"name", 0o100644).expect("create");
    fs.rename(a, b"new", b, b"name")
        .expect("replace across directories");
    assert_eq!(names(&fs, b).get(&b"name"[..]), Some(&moved));
    assert!(names(&fs, a).is_empty());
    assert!(is_free(&fs, old), "the replaced inode was not freed");
    let (next, _) = fs.create_file(root, b"next", 0o100644).expect("create");
    assert_eq!(next, old, "the freed slot is not found again by a create");
}

#[test]
fn a_file_replaces_a_file_in_the_same_directory() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (d, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let (moved, _) = fs.create_file(d, b"from", 0o100644).expect("create");
    let (old, _) = fs.create_file(d, b"to", 0o100644).expect("create");
    fs.rename(d, b"from", d, b"to")
        .expect("replace in one directory");
    let got = names(&fs, d);
    assert_eq!(got.len(), 1);
    assert_eq!(got.get(&b"to"[..]), Some(&moved));
    assert!(is_free(&fs, old));
}

#[test]
fn a_replaced_file_with_data_gives_its_blocks_back() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (moved, _) = fs.create_file(root, b"new", 0o100644).expect("create");
    let (old, _) = fs.create_file(root, b"full", 0o100644).expect("create");
    let bs = fs.superblock().blocksize as usize;
    fs.write_into_empty_file(old, &vec![0x77; 8 * bs])
        .expect("fill");
    let fdblocks = |fs: &Filesystem| -> u64 {
        (0..fs.superblock().agcount)
            .map(|ag| u64::from(fs.read_agf(ag).expect("agf").freeblks))
            .sum()
    };
    let before = fdblocks(&fs);
    fs.rename(root, b"new", root, b"full")
        .expect("replace a file with data");
    assert!(is_free(&fs, old));
    assert!(
        fdblocks(&fs) >= before + 8,
        "the replaced file's eight blocks did not come back"
    );
    assert_eq!(names(&fs, root).get(&b"full"[..]), Some(&moved));
}

#[test]
fn a_directory_replaces_an_empty_directory() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir");
    let (moved, _) = fs.create_directory(a, b"sub", 0o040755).expect("mkdir");
    fs.create_file(moved, b"inside", 0o100644).expect("create");
    let (empty, _) = fs.create_directory(b, b"sub", 0o040755).expect("mkdir");
    let links = |ino: u64| fs.read_inode(ino).expect("inode").nlink;
    assert_eq!((links(a), links(b)), (3, 3));
    fs.rename(a, b"sub", b, b"sub")
        .expect("replace an empty directory");
    assert!(is_free(&fs, empty), "the replaced directory was not freed");
    assert_eq!(names(&fs, b).get(&b"sub"[..]), Some(&moved));
    assert_eq!(
        (links(a), links(b)),
        (2, 3),
        "the link counts after a directory replaced one"
    );
    assert!(names(&fs, moved).contains_key(&b"inside"[..]));
}

#[test]
fn what_posix_forbids_is_refused_and_nothing_is_written() {
    let (dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"dir", 0o040755).expect("mkdir");
    let (full, _) = fs.create_directory(root, b"full", 0o040755).expect("mkdir");
    fs.create_file(full, b"x", 0o100644).expect("create");
    fs.create_file(root, b"file", 0o100644).expect("create");
    let writes = dev.writes.load(Ordering::SeqCst);
    assert!(matches!(
        fs.rename(root, b"file", root, b"dir"),
        Err(Error::NotAFile)
    ));
    assert!(matches!(
        fs.rename(root, b"dir", root, b"file"),
        Err(Error::NotADirectory)
    ));
    assert!(matches!(
        fs.rename(root, b"dir", root, b"full"),
        Err(Error::DirectoryNotEmpty)
    ));
    assert_eq!(dev.writes.load(Ordering::SeqCst), writes, "a refusal wrote");
    assert!(names(&fs, root).len() == 3);
    let _ = dir;
}

#[test]
fn two_names_for_one_inode_is_a_rename_that_does_nothing() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    fs.create_file(root, b"same", 0o100644).expect("create");
    assert_eq!(
        fs.rename(root, b"same", root, b"same")
            .expect("rename onto itself"),
        0
    );
}
