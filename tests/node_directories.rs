//! A directory grows past one leaf of index into node form, and shrinks
//! back, as names are added, renamed and removed (#367).
//!
//! The volume has 1 KiB blocks under 4 KiB directory blocks, as
//! `mkfs.xfs` makes them, so a node's pointers count filesystem blocks
//! and step four at a time between directory blocks. Most names are hard
//! links to one file, which grows the directory without growing the inode
//! tree alongside it. Each step is checked from the outside: every name
//! lists once, every name looks up to the inode it lists, nothing else
//! lists, and the index has the shape the step should have given it.
//!
//! A second level of nodes needs some hundred thousand names under 4 KiB
//! directory blocks, and this crate's `mkfs` makes no smaller ones, so the
//! two-level tree and what the kernel and `xfs_repair` make of all of it
//! is `tests/node_directories_oracle.rs`, on 1 KiB directory blocks.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 400 * 1024 * 1024;
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
    let options = fs_xfs::mkfs::Options {
        block_size: 1024,
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn name(i: usize) -> Vec<u8> {
    format!("a-file-in-a-node-form-directory-{i:05}").into_bytes()
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

/// The directory's index root, when it has one: its magic and, for a
/// node, its level.
fn root(fs: &Filesystem, dir: u64) -> Option<(u16, u16)> {
    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    if format!("{:?}", inode.format) == "Local" {
        return None;
    }
    let bs = u64::from(fs.superblock().blocksize);
    let leaf = (1u64 << 35) / bs;
    let e = fs
        .data_extents(&inode, &raw)
        .expect("extents")
        .into_iter()
        .find(|e| e.startoff <= leaf && leaf < e.startoff + e.blockcount)?;
    let mut block = vec![0u8; bs as usize];
    fs.device()
        .read_at(
            fs.superblock()
                .fsblock_offset(e.startblock + (leaf - e.startoff)),
            &mut block,
        )
        .expect("read the root");
    let magic = u16::from_be_bytes([block[8], block[9]]);
    Some((magic, u16::from_be_bytes([block[58], block[59]])))
}

const NODE: u16 = 0x3ebe;
const LEAFN: u16 = 0x3dff;
const LEAF1: u16 = 0x3df1;

/// Links to `file` named `name(0..n)`, checking the shape on the way:
/// leaf form, then a node over leaves.
fn grow(fs: &Filesystem, dir: u64, file: u64, n: usize) -> BTreeMap<Vec<u8>, u64> {
    let mut want = BTreeMap::new();
    let mut seen = Vec::new();
    for i in 0..n {
        fs.link(file, dir, &name(i)).expect("link");
        want.insert(name(i), file);
        let shape = root(fs, dir).map(|(magic, level)| match magic {
            NODE => format!("node {level}"),
            LEAFN => "leafn".to_string(),
            LEAF1 => "leaf".to_string(),
            other => format!("{other:#x}"),
        });
        if seen.last() != Some(&shape) {
            seen.push(shape);
        }
    }
    assert!(seen.contains(&Some("leaf".into())), "{seen:?}");
    assert_eq!(seen.last(), Some(&Some("node 1".into())), "{seen:?}");
    want
}

/// A directory `d` and a file `f` beside it to link into it.
fn dir_and_file(fs: &Filesystem) -> (u64, u64) {
    let root_ino = fs.superblock().rootino;
    let (dir, _) = fs
        .create_directory(root_ino, b"d", 0o040755)
        .expect("mkdir");
    let (file, _) = fs.create_file(root_ino, b"f", 0o100644).expect("create");
    (dir, file)
}

fn without_dots(mut m: BTreeMap<Vec<u8>, u64>) -> BTreeMap<Vec<u8>, u64> {
    m.remove(&b"."[..]);
    m.remove(&b".."[..]);
    m
}

#[test]
fn a_directory_grows_through_node_form_and_its_map_into_a_tree() {
    let fs = mounted();
    let (dir, file) = dir_and_file(&fs);
    let mut want = grow(&fs, dir, file, 6000);
    let (inode, _) = fs.read_inode_raw(dir).expect("directory");
    assert_eq!(
        format!("{:?}", inode.format),
        "Btree",
        "its map outgrew the inode"
    );
    // Files made in it, not only links, and a name that is there refused.
    for i in 0..50 {
        let n = format!("made-{i}").into_bytes();
        let (ino, _) = fs.create_file(dir, &n, 0o100644).expect("create");
        want.insert(n, ino);
    }
    assert!(matches!(
        fs.create_file(dir, &name(0), 0o100644),
        Err(fs_xfs::Error::AlreadyExists)
    ));
    assert_eq!(without_dots(listed(&fs, dir)), want);
}

#[test]
fn removing_names_takes_a_node_directory_back_to_one_block() {
    let fs = mounted();
    let (dir, file) = dir_and_file(&fs);
    let mut want = grow(&fs, dir, file, 4000);
    let (before, _) = fs.read_inode_raw(dir).expect("directory");
    // Every other name first, which empties no leaf, then the rest from
    // the end, which empties leaves, collapses the root and gives back
    // data blocks from the middle and the end.
    let names: Vec<Vec<u8>> = want.keys().cloned().collect();
    let order: Vec<&Vec<u8>> = names
        .iter()
        .step_by(2)
        .chain(names.iter().skip(1).step_by(2).rev())
        .collect();
    let mut shapes = Vec::new();
    for (n, gone) in order.iter().enumerate() {
        if want.len() == 3 {
            break;
        }
        fs.unlink_file(dir, gone).expect("unlink");
        want.remove(*gone);
        let shape = root(&fs, dir).map(|r| r.0);
        if shapes.last() != Some(&shape) {
            shapes.push(shape);
        }
        if n % 499 == 0 {
            assert_eq!(without_dots(listed(&fs, dir)), want, "after {n} removals");
        }
    }
    assert_eq!(without_dots(listed(&fs, dir)), want);
    // A root left with one leaf may be laid out in leaf form by the same
    // removal, so the single leaf is not always seen.
    shapes.retain(|s| *s != Some(LEAFN));
    assert_eq!(
        shapes,
        vec![Some(NODE), Some(LEAF1), None],
        "node, leaf form, one block"
    );
    let (after, _) = fs.read_inode_raw(dir).expect("directory");
    assert!(after.nblocks < before.nblocks);
    assert_eq!(after.size, 4096);
    assert_eq!(format!("{:?}", after.format), "Extents");
}

#[test]
fn names_renamed_and_moved_in_a_node_directory_are_found_by_their_new_names() {
    let fs = mounted();
    let (dir, file) = dir_and_file(&fs);
    let root_ino = fs.superblock().rootino;
    let (other, _) = fs
        .create_directory(root_ino, b"o", 0o040755)
        .expect("mkdir");
    let mut want = grow(&fs, dir, file, 1500);
    let mut moved = BTreeMap::new();
    for i in (0..1500).step_by(7) {
        let ino = want.remove(&name(i)).expect("there");
        let new = format!("renamed-{i}").into_bytes();
        fs.rename_in_directory(dir, &name(i), &new)
            .expect("rename in place");
        want.insert(new, ino);
    }
    for i in (3..1500).step_by(11) {
        let Some(ino) = want.remove(&name(i)) else {
            continue;
        };
        fs.rename(dir, &name(i), other, &name(i)).expect("move out");
        moved.insert(name(i), ino);
    }
    assert_eq!(without_dots(listed(&fs, dir)), want);
    assert_eq!(without_dots(listed(&fs, other)), moved);
    // A subdirectory moved into the node-form directory points `..` at it,
    // and one moved out of it takes a name with it.
    let (sub, _) = fs.create_directory(other, b"sub", 0o040755).expect("mkdir");
    fs.rename(other, b"sub", dir, b"sub")
        .expect("move a directory in");
    let (sub_inode, sub_raw) = fs.read_inode_raw(sub).expect("sub");
    let (start, end) = sub_inode.data_fork_range(usize::from(fs.superblock().inodesize));
    let parent = fs_xfs::dir::read_short_form(&sub_inode, &sub_raw[start..end], fs.superblock())
        .expect("short form")
        .parent_ino;
    assert_eq!(parent, dir);
    want.insert(b"sub".to_vec(), sub);
    assert_eq!(without_dots(listed(&fs, dir)), want);
}
