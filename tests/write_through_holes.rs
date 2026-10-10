//! A write lands in holes and past the end of a file, and reads back as
//! the bytes written with zeros everywhere else (#387).
//!
//! `write_at` overwrites bytes that already exist and refuses everything
//! that needs a metadata change. `Filesystem::write` is the journalled
//! write that makes those changes: blocks for a hole, a larger size for a
//! write past the end, and written extents out of unwritten ones. Every
//! byte the write did not cover reads back as zero: the rest of a block
//! it allocated, the gap between the old end and the new data, and the
//! old last block's tail past the old end.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on a sparse in-memory device, and each file is
//! compared byte for byte with a model built alongside it. Unwritten
//! extents, which only the kernel makes, and the kernel's own reading of
//! every case are `tests/write_holes_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::alloc_btree::{walk_from_agf, Order};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: u64 = 512;

/// What a sector nobody has written reads as: not zero, so that a block
/// taken without being zero-filled reads back as this rather than as the
/// zeros a fresh device would hide the omission behind.
const STALE: u8 = 0xEE;

struct Sparse(Mutex<BTreeMap<u64, Vec<u8>>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = sectors
                .get(&(at / SECTOR))
                .map_or(STALE, |s| s[(at % SECTOR) as usize]);
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
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            sectors
                .entry(at / SECTOR)
                .or_insert_with(|| vec![STALE; SECTOR as usize])[(at % SECTOR) as usize] = byte;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A mounted volume on a device whose unwritten sectors read as
/// [`STALE`], so every free block holds bytes a skipped zero-fill would
/// expose.
fn mounted() -> (Arc<Sparse>, Filesystem) {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    // The first free blocks are inside the megabyte mkfs zeroed, so every
    // free block, read off the volume's own free-space tree, is dirtied by
    // hand before the volume is mounted for writing.
    {
        let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount");
        let sb = fs.superblock().clone();
        let bs = u64::from(sb.blocksize);
        for ag in 0..sb.agcount {
            let agf = fs.read_agf(ag).expect("agf");
            let ag_start = u64::from(ag) * u64::from(sb.agblocks) * bs;
            let read = |b: u32| -> fs_xfs::Result<Vec<u8>> {
                let mut raw = vec![0u8; bs as usize];
                dev.read_at(ag_start + u64::from(b) * bs, &mut raw)?;
                Ok(raw)
            };
            let free = walk_from_agf(&sb, &agf, Order::ByBlock, read).expect("free space");
            for run in free {
                let at = ag_start + u64::from(run.startblock) * bs;
                dev.write_at(at, &vec![STALE; (u64::from(run.blockcount) * bs) as usize])
                    .expect("dirty");
            }
        }
    }
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    (dev, fs)
}

/// A file and the bytes it should hold.
struct Modelled {
    ino: u64,
    want: Vec<u8>,
}

impl Modelled {
    fn new(fs: &Filesystem, name: &[u8]) -> Self {
        let root = fs.superblock().rootino;
        let (ino, _) = fs.create_file(root, name, 0o100644).expect("create");
        Modelled {
            ino,
            want: Vec::new(),
        }
    }

    fn write(&mut self, fs: &Filesystem, offset: u64, data: &[u8]) {
        fs.write(self.ino, offset, data)
            .unwrap_or_else(|e| panic!("write {} bytes at {offset}: {e:?}", data.len()));
        let end = offset as usize + data.len();
        if self.want.len() < end {
            self.want.resize(end, 0);
        }
        self.want[offset as usize..end].copy_from_slice(data);
    }

    fn check(&self, fs: &Filesystem, what: &str) {
        let (inode, raw) = fs.read_inode_raw(self.ino).expect("read inode");
        assert_eq!(inode.size, self.want.len() as u64, "{what}: size");
        let got = fs.read_file(&inode, &raw).expect("read file");
        if got != self.want {
            let at = got
                .iter()
                .zip(&self.want)
                .position(|(a, b)| a != b)
                .unwrap_or(got.len().min(self.want.len()));
            panic!(
                "{what}: the file differs from the model at byte {at} \
                 (got {:?}, want {:?})",
                got.get(at),
                self.want.get(at)
            );
        }
    }
}

fn pattern(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed) | 1)
        .collect()
}

#[test]
fn a_write_into_an_empty_file_at_an_offset_leaves_a_leading_hole() {
    let (_dev, fs) = mounted();
    let bs = u64::from(fs.superblock().blocksize);
    let mut f = Modelled::new(&fs, b"leading");
    f.write(&fs, 3 * bs + 100, &pattern(500, 7));
    f.check(&fs, "a write three blocks in");
}

#[test]
fn a_write_past_the_end_zero_fills_the_gap_and_the_old_tail() {
    let (_dev, fs) = mounted();
    let bs = u64::from(fs.superblock().blocksize);
    let mut f = Modelled::new(&fs, b"grow");
    f.write(&fs, 0, &pattern(1000, 1));
    // Inside the same block as the old end, past it.
    f.write(&fs, 1500, &pattern(200, 2));
    f.check(&fs, "a write past the end, in the last block");
    // Several blocks past the end.
    f.write(&fs, 5 * bs + 17, &pattern(300, 3));
    f.check(&fs, "a write several blocks past the end");
}

#[test]
fn a_write_into_a_hole_between_extents_fills_only_that_hole() {
    let (_dev, fs) = mounted();
    let bs = u64::from(fs.superblock().blocksize);
    let mut f = Modelled::new(&fs, b"middle");
    f.write(&fs, 0, &pattern(bs as usize, 4));
    f.write(&fs, 8 * bs, &pattern(bs as usize, 5));
    // Straddling the hole's interior, partial at both ends.
    f.write(&fs, 3 * bs + 10, &pattern(2 * bs as usize, 6));
    f.check(&fs, "a write inside a hole");
    let inode = fs.read_inode(f.ino).expect("inode");
    assert_eq!(
        inode.nblocks,
        1 + 1 + 3,
        "blocks: the two ends and the three written"
    );
}

#[test]
fn a_write_across_data_and_holes_overwrites_and_allocates() {
    let (_dev, fs) = mounted();
    let bs = u64::from(fs.superblock().blocksize);
    let mut f = Modelled::new(&fs, b"across");
    f.write(&fs, bs, &pattern(bs as usize, 8));
    f.write(&fs, 4 * bs, &pattern(bs as usize, 9));
    // From the leading hole, over the first extent, the hole between and
    // the second extent, and past the end.
    f.write(&fs, 100, &pattern(6 * bs as usize, 10));
    f.check(&fs, "a write across data and holes");
}

#[test]
fn many_writes_on_one_mount_each_build_on_the_last() {
    let (_dev, fs) = mounted();
    let bs = u64::from(fs.superblock().blocksize);
    let mut f = Modelled::new(&fs, b"many");
    for (i, at) in [7 * bs, 0, 3 * bs + 5, 2 * bs - 1, 9 * bs + 9]
        .into_iter()
        .enumerate()
    {
        f.write(&fs, at, &pattern(bs as usize / 2 + i * 37, i as u8));
        f.check(&fs, &format!("after write {i}"));
    }
}

#[test]
fn what_it_cannot_do_is_refused_before_it_writes() {
    let (_dev, fs) = mounted();
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    assert!(matches!(fs.write(dir, 0, b"x"), Err(Error::NotAFile)));
    let f = Modelled::new(&fs, b"overflow");
    let huge = fs.write(f.ino, u64::MAX - 1, b"xyz");
    assert!(huge.is_err(), "a write past 2^64: {huge:?}");
    assert_eq!(
        fs.read_inode(f.ino).expect("inode").size,
        0,
        "a refusal changed the size"
    );
}
