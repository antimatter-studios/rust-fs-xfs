//! A file has several names, loses them one at a time, and is freed with
//! everything it holds when the last one goes (#384).
//!
//! `Filesystem::link` gives an inode another name in any directory and
//! raises its link count. `unlink_file` takes one name away: a file with
//! another name lives on a link fewer, and the last name frees the inode,
//! its blocks and its quota in the same record.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device. What the kernel and
//! `xfs_repair` make of the same links is `tests/hardlinks_oracle.rs`.

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

fn filesystem(block_size: u32) -> Filesystem {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(
        dev.as_ref(),
        &fs_xfs::mkfs::Options {
            block_size,
            ..fs_xfs::mkfs::Options::default()
        },
    )
    .expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn free_blocks(fs: &Filesystem) -> u64 {
    (0..fs.superblock().agcount)
        .map(|ag| u64::from(fs.read_agf(ag).expect("agf").freeblks))
        .sum()
}

fn contents(fs: &Filesystem, path: &str) -> Vec<u8> {
    let ino = fs.lookup_path(path).expect(path).ino;
    let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
    fs.read_file(&inode, &raw).expect("read")
}

/// Carried forward from the abandoned `feat/xfs-v5-384` branch, where it
/// was written first and never had an implementation to pass.
#[test]
fn final_unlink_frees_a_populated_inode_and_its_blocks() {
    for blocksize in [1024, 4096] {
        let fs = filesystem(blocksize);
        let root = fs.superblock().rootino;
        let ino = fs.create_file(root, b"victim", 0o600).unwrap().0;
        let payload = vec![0x5a; blocksize as usize + 17];
        fs.write_into_empty_file(ino, &payload).unwrap();
        let before = fs.read_inode(ino).unwrap();
        let agno = fs.superblock().split_ino(ino).0;
        let free_inodes = fs.read_agi(agno).unwrap().freecount;
        let free_blocks_before = free_blocks(&fs);
        fs.unlink_file(root, b"victim")
            .expect("the final unlink must free file data with the inode");
        assert!(matches!(fs.lookup_path("/victim"), Err(Error::NotFound)));
        assert_eq!(fs.read_agi(agno).unwrap().freecount, free_inodes + 1);
        assert!(
            free_blocks(&fs) >= free_blocks_before + before.nblocks,
            "{blocksize}: the file's {} blocks did not come back",
            before.nblocks
        );
        let freed = fs.read_inode(ino).unwrap();
        assert_eq!((freed.mode, freed.nlink, freed.nblocks), (0, 0, 0));
        assert_eq!(freed.gen, before.gen.wrapping_add(1));
    }
}

#[test]
fn a_file_keeps_its_contents_through_each_name_but_the_last() {
    let fs = filesystem(4096);
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir a");
    let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir b");
    let (ino, _) = fs.create_file(a, b"one", 0o100644).expect("create");
    let data = vec![0x3c; 10_000];
    fs.write_into_empty_file(ino, &data).expect("fill");
    fs.link(ino, b, b"two")
        .expect("link into another directory");
    fs.link(ino, a, b"three")
        .expect("link into the same directory");
    assert_eq!(fs.read_inode(ino).expect("inode").nlink, 3);
    for path in ["/a/one", "/b/two", "/a/three"] {
        assert_eq!(fs.lookup_path(path).expect(path).ino, ino, "{path}");
        assert_eq!(contents(&fs, path), data, "{path}");
    }
    let blocks = free_blocks(&fs);
    fs.unlink_file(a, b"one").expect("unlink one of three");
    assert_eq!(fs.read_inode(ino).expect("inode").nlink, 2);
    assert_eq!(
        contents(&fs, "/b/two"),
        data,
        "a name was removed and the data with it"
    );
    assert_eq!(
        free_blocks(&fs),
        blocks,
        "blocks came back while a name was left"
    );
    fs.unlink_file(b, b"two").expect("unlink two of three");
    assert_eq!(contents(&fs, "/a/three"), data);
    fs.unlink_file(a, b"three").expect("the last name");
    let freed = fs.read_inode(ino).expect("inode");
    assert_eq!((freed.mode, freed.nlink, freed.nblocks), (0, 0, 0));
    assert!(
        free_blocks(&fs) > blocks,
        "the last name went and the blocks stayed"
    );
}

#[test]
fn a_link_lands_in_a_leaf_form_directory() {
    let fs = filesystem(4096);
    let root = fs.superblock().rootino;
    let (big, _) = fs.create_directory(root, b"big", 0o040755).expect("mkdir");
    for i in 0..300 {
        fs.create_file(
            big,
            format!("name-in-a-big-directory-{i:04}").as_bytes(),
            0o100644,
        )
        .expect("create");
    }
    let (ino, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    fs.link(ino, big, b"linked-into-the-big-one").expect("link");
    assert_eq!(
        fs.lookup_path("/big/linked-into-the-big-one")
            .expect("lookup")
            .ino,
        ino
    );
    assert_eq!(fs.read_inode(ino).expect("inode").nlink, 2);
}

#[test]
fn what_a_link_cannot_do_is_refused() {
    let fs = filesystem(4096);
    let root = fs.superblock().rootino;
    let (d, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let (f, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    assert!(
        fs.link(d, root, b"d2").is_err(),
        "a directory was given a second name"
    );
    assert!(matches!(fs.link(f, root, b"d"), Err(Error::AlreadyExists)));
    assert!(matches!(fs.link(f, f, b"x"), Err(Error::NotADirectory)));
    assert_eq!(fs.read_inode(f).expect("inode").nlink, 1);
}
