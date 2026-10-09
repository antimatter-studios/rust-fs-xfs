//! A name moves from one directory to another, and a directory takes its
//! `..` with it (#382).
//!
//! `Filesystem::rename(from_dir, from, to_dir, to)` removes the name from
//! one directory and adds it to the other in one record. A directory that
//! moves names its new parent in `..`, its old parent loses a link and
//! its new one gains one. Each directory changes in whatever form it is
//! in, inline or past its inode.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device. What the kernel and
//! `xfs_repair` make of the same moves is
//! `tests/cross_directory_rename_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
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

fn names(fs: &Filesystem, dir: u64) -> BTreeMap<Vec<u8>, u64> {
    let (inode, raw) = fs.read_inode_raw(dir).expect("dir");
    fs.read_dir(&inode, &raw)
        .expect("readdir")
        .into_iter()
        .map(|e| (e.name, e.ino))
        .collect()
}

/// The inode a short-form directory's `..` names.
fn parent_of(fs: &Filesystem, dir: u64) -> u64 {
    let (inode, raw) = fs.read_inode_raw(dir).expect("dir");
    let (start, end) = inode.data_fork_range(usize::from(fs.superblock().inodesize));
    fs_xfs::dir::read_short_form(&inode, &raw[start..end], fs.superblock())
        .expect("short form")
        .parent_ino
}

fn nlink(fs: &Filesystem, ino: u64) -> u32 {
    fs.read_inode(ino).expect("inode").nlink
}

#[test]
fn a_file_moves_between_directories() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir a");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir b");
    let (file, _) = fs.create_file(a, b"file", 0o100644).expect("create");
    fs.rename(a, b"file", b, b"moved").expect("move");
    assert!(names(&fs, a).is_empty(), "the name stayed in the source");
    assert_eq!(names(&fs, b).get(&b"moved"[..]), Some(&file));
    assert_eq!((nlink(&fs, a), nlink(&fs, b), nlink(&fs, file)), (2, 2, 1));
}

#[test]
fn a_directory_moves_and_takes_its_dotdot_with_it() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir a");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir b");
    let (sub, _) = fs.create_directory(a, b"sub", 0o040755).expect("mkdir sub");
    fs.create_file(sub, b"inside", 0o100644)
        .expect("create inside");
    assert_eq!((nlink(&fs, a), nlink(&fs, b)), (3, 2));
    fs.rename(a, b"sub", b, b"sub").expect("move a directory");
    assert_eq!(
        parent_of(&fs, sub),
        b,
        "the moved directory's .. still names its old parent"
    );
    assert_eq!((nlink(&fs, a), nlink(&fs, b), nlink(&fs, sub)), (2, 3, 2));
    assert!(
        names(&fs, sub).contains_key(&b"inside"[..]),
        "the directory lost its contents"
    );
}

#[test]
fn names_move_into_and_out_of_leaf_form_directories() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (big, _) = fs
        .create_directory(root, b"big", 0o040755)
        .expect("mkdir big");
    let (small, _) = fs
        .create_directory(root, b"small", 0o040755)
        .expect("mkdir small");
    for i in 0..300 {
        fs.create_file(
            big,
            format!("name-in-a-big-directory-{i:04}").as_bytes(),
            0o100644,
        )
        .expect("create");
    }
    let before = names(&fs, big);
    let out = b"name-in-a-big-directory-0123".to_vec();
    let ino = before[&out];
    fs.rename(big, &out, small, b"out")
        .expect("out of leaf form");
    let (into, _) = fs.create_file(small, b"into", 0o100644).expect("create");
    fs.rename(small, b"into", big, b"into-the-big-one")
        .expect("into leaf form");
    let mut want = before.clone();
    want.remove(&out);
    want.insert(b"into-the-big-one".to_vec(), into);
    assert_eq!(names(&fs, big), want);
    assert_eq!(names(&fs, small).get(&b"out"[..]), Some(&ino));
}

#[test]
fn a_directory_whose_name_does_not_fit_its_inode_moves_into_block_form() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (src, _) = fs.create_directory(root, b"src", 0o040755).expect("mkdir");
    let (dst, _) = fs.create_directory(root, b"dst", 0o040755).expect("mkdir");
    // Fill the destination's inode with long names, so one more cannot fit.
    let mut want = BTreeMap::new();
    let mut i = 0;
    loop {
        let name = format!("a-rather-long-name-to-fill-an-inode-{i:03}");
        let (ino, _) = fs
            .create_file(dst, name.as_bytes(), 0o100644)
            .expect("create");
        want.insert(name.into_bytes(), ino);
        i += 1;
        if fs.read_inode(dst).expect("dst").format != fs_xfs::inode::Format::Local || i > 5 {
            break;
        }
    }
    let (moved, _) = fs.create_file(src, b"m", 0o100644).expect("create");
    let long = b"one-more-long-name-that-will-not-fit-in-what-is-left-of-the-inode".to_vec();
    fs.rename(src, b"m", dst, &long).expect("move");
    want.insert(long, moved);
    assert_eq!(names(&fs, dst), want);
    assert_ne!(
        fs.read_inode(dst).expect("dst").format,
        fs_xfs::inode::Format::Local,
        "the destination never left its inode, so this did not test the conversion"
    );
}

#[test]
fn what_a_move_cannot_do_is_refused_before_it_writes() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir a");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir b");
    let (inner, _) = fs
        .create_directory(a, b"inner", 0o040755)
        .expect("mkdir inner");
    fs.create_file(a, b"x", 0o100644).expect("create x");
    fs.create_file(b, b"x", 0o100644).expect("create x");
    assert!(matches!(
        fs.rename(a, b"x", b, b"x"),
        Err(Error::AlreadyExists)
    ));
    assert!(matches!(
        fs.rename(a, b"nope", b, b"y"),
        Err(Error::NotFound)
    ));
    let loop_ = fs.rename(root, b"a", inner, b"a");
    assert!(
        loop_.is_err(),
        "a directory moved beneath itself: {loop_:?}"
    );
    assert!(
        names(&fs, root).contains_key(&b"a"[..]),
        "a refused move changed the tree"
    );
    assert_eq!(parent_of(&fs, inner), a);
}
