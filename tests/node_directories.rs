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
//! A second level of nodes needs some 175,000 names under 4 KiB directory
//! blocks, and neither `mkfs.xfs` nor this crate's `mkfs` makes smaller
//! ones, so the two-level tree and what the kernel and `xfs_repair` make
//! of all of it is `tests/node_directories_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 400 * 1024 * 1024;
const SECTOR: usize = 512;

/// Sectors written, and how many bytes have been read.
struct Sparse(Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>, AtomicU64);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.1.fetch_add(buf.len() as u64, Ordering::Relaxed);
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
    mounted_on().0
}

/// A mounted volume, and the device under it.
fn mounted_on() -> (Filesystem, Arc<Sparse>) {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new()), AtomicU64::new(0)));
    let options = fs_xfs::mkfs::Options {
        block_size: 1024,
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (fs, dev)
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

/// Bytes `op` read from the device.
fn read_by(dev: &Sparse, op: impl FnOnce()) -> u64 {
    let before = dev.1.load(Ordering::Relaxed);
    op();
    dev.1.load(Ordering::Relaxed) - before
}

#[test]
fn a_link_or_a_move_reads_no_more_in_a_directory_four_times_the_size() {
    let (fs, dev) = mounted_on();
    let (dir, file) = dir_and_file(&fs);
    let (other, _) = fs
        .create_directory(fs.superblock().rootino, b"e", 0o040755)
        .expect("mkdir");
    let mut cost = Vec::new();
    let mut n = 0;
    for size in [3000, 12000] {
        while n < size {
            fs.link(file, dir, &name(n)).expect("link");
            n += 1;
        }
        let link = read_by(&dev, || {
            fs.link(file, dir, format!("one-more-at-{size}").as_bytes())
                .expect("link");
        });
        let moved = read_by(&dev, || {
            fs.rename(dir, &name(size - 1), other, &name(size - 1))
                .expect("move");
        });
        cost.push((size, link, moved));
    }
    let ((_, link, moved), (_, link4, moved4)) = (cost[0], cost[1]);
    assert!(
        link4 < 2 * link && moved4 < 2 * moved,
        "(names, bytes one link reads, bytes one move reads): {cost:?}"
    );
}

/// Push everything the mount logged to its home blocks, then log one more
/// change, so a later mount replays that record alone and reads the
/// directory from disk, where a test can damage it (#443).
fn settle(fs: &Filesystem) {
    fs.sync().expect("sync");
    fs.create_file(fs.superblock().rootino, b"settled", 0o100644)
        .expect("a record after the push");
}

/// Directory `dir`'s directory block starting at file block `dablk`: each
/// filesystem block's device offset, and the block's bytes.
fn dir_block(fs: &Filesystem, dir: u64, dablk: u64) -> (Vec<u64>, Vec<u8>) {
    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    let extents = fs.data_extents(&inode, &raw).expect("extents");
    let bs = u64::from(fs.superblock().blocksize);
    let per = u64::from(fs.superblock().dirblocksize()) / bs;
    let mut at = Vec::new();
    let mut bytes = Vec::new();
    for fb in dablk..dablk + per {
        let e = extents
            .iter()
            .find(|e| e.startoff <= fb && fb < e.startoff + e.blockcount)
            .expect("mapped");
        let off = fs
            .superblock()
            .fsblock_offset(e.startblock + (fb - e.startoff));
        let mut b = vec![0u8; bs as usize];
        fs.device().read_at(off, &mut b).expect("read");
        at.push(off);
        bytes.extend_from_slice(&b);
    }
    (at, bytes)
}

fn put_dir_block(dev: &Sparse, at: &[u64], mut bytes: Vec<u8>) {
    fs_xfs::group_write::restamp_crc(&mut bytes, 12);
    let bs = bytes.len() / at.len();
    for (i, off) in at.iter().enumerate() {
        dev.write_at(*off, &bytes[i * bs..(i + 1) * bs])
            .expect("write");
    }
}

/// `lookup_path(path)` on a fresh read-only mount, given 20 seconds.
fn looked_up(dev: &Arc<Sparse>, path: String) -> Result<String, &'static str> {
    let (tx, rx) = std::sync::mpsc::channel();
    let dev = dev.clone();
    std::thread::spawn(move || {
        let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
        let _ = tx.send(format!("{:?}", fs.lookup_path(&path).map(|i| i.ino)));
    });
    rx.recv_timeout(std::time::Duration::from_secs(20))
        .map_err(|_| "no answer in 20 seconds: the lookup is looping")
}

const NODE_HDR: usize = 64;
const LEAF_HDR: usize = 64;

#[test]
fn a_node_that_names_itself_as_its_child_is_refused_not_followed() {
    let (fs, dev) = mounted_on();
    let (dir, file) = dir_and_file(&fs);
    grow(&fs, dir, file, 4000);
    settle(&fs);
    let bs = u64::from(fs.superblock().blocksize);
    let root = (1u64 << 35) / bs;
    let (at, mut block) = dir_block(&fs, dir, root);
    drop(fs);
    assert_eq!(u16::from_be_bytes([block[8], block[9]]), NODE, "not a node");
    let count = usize::from(u16::from_be_bytes([block[56], block[57]]));
    for i in 0..count {
        let before = NODE_HDR + i * 8 + 4;
        block[before..before + 4].copy_from_slice(&(root as u32).to_be_bytes());
    }
    put_dir_block(&dev, &at, block);
    let got = looked_up(&dev, format!("/d/{}", String::from_utf8_lossy(&name(7))));
    assert!(
        matches!(&got, Ok(r) if r.starts_with("Err")),
        "a node naming itself must be refused: {got:?}"
    );
}

#[test]
fn a_leaf_chain_that_loops_is_refused_not_followed() {
    let (fs, dev) = mounted_on();
    let (dir, file) = dir_and_file(&fs);
    grow(&fs, dir, file, 4000);
    settle(&fs);
    let bs = u64::from(fs.superblock().blocksize);
    let per = u64::from(fs.superblock().dirblocksize()) / bs;
    let dbs = u64::from(fs.superblock().dirblocksize());
    let (_, node) = dir_block(&fs, dir, (1u64 << 35) / bs);
    let first = u64::from(u32::from_be_bytes(
        node[NODE_HDR + 4..NODE_HDR + 8].try_into().unwrap(),
    ));
    let (at, mut leaf) = dir_block(&fs, dir, first);
    assert_eq!(
        u16::from_be_bytes([leaf[8], leaf[9]]),
        LEAFN,
        "not a node leaf"
    );
    let count = usize::from(u16::from_be_bytes([leaf[56], leaf[57]]));
    let last = LEAF_HDR + (count - 1) * 8;
    let address = u64::from(u32::from_be_bytes(
        leaf[last + 4..last + 8].try_into().unwrap(),
    ));
    assert_ne!(address, 0, "the leaf's last record is stale");
    // The name the last record points at, read from its data block: a
    // lookup of it has to look past this leaf for more of its hash.
    let (db, offset) = (address * 8 / dbs, (address * 8 % dbs) as usize);
    let (_, data) = dir_block(&fs, dir, db * per);
    let len = usize::from(data[offset + 8]);
    let wanted = String::from_utf8_lossy(&data[offset + 9..offset + 9 + len]).into_owned();
    drop(fs);
    leaf[0..4].copy_from_slice(&(first as u32).to_be_bytes());
    put_dir_block(&dev, &at, leaf);
    let got = looked_up(&dev, format!("/d/{wanted}"));
    assert!(
        matches!(&got, Ok(r) if r.starts_with("Err")),
        "a leaf whose forward pointer names itself must be refused: {got:?}"
    );
}
