//! A write larger than the inode's own allocation group is placed across
//! groups (#388).
//!
//! `Filesystem::write` gives a hole blocks one free run at a time: from
//! the inode's own group while it has free runs, then from each other
//! group in turn. One write of a mebibyte more than the root's group has
//! free, read off that group's own free-space tree, has to be placed
//! across groups.
//!
//! The volume is made by this crate's own `mkfs` on an in-memory device.
//! What the kernel makes of a file placed across groups is
//! `tests/write_across_groups_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::alloc_btree::{walk_from_agf, Order};
use fs_xfs::Filesystem;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

/// Only the sectors written are kept, copied a sector at a time.
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
            let s = sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]));
            s[within..within + n].copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| ((i / 4096) as u8) ^ (i as u8).wrapping_mul(13) | 1)
        .collect()
}

#[test]
fn a_write_larger_than_the_inodes_group_spans_groups() {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    let opts = fs_xfs::mkfs::Options {
        agcount: Some(4),
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &opts).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    let sb = fs.superblock().clone();

    let root = sb.rootino;
    let (ino, _) = fs.create_file(root, b"big", 0o100644).expect("create");
    // One mebibyte more than the root's group has free, read off its own
    // free-space tree, so the write cannot fit in it however it is laid out.
    let bs = u64::from(sb.blocksize);
    let (home, _, _) = sb.split_ino(root);
    let agf = fs.read_agf(home).expect("agf");
    let ag_start = u64::from(home) * u64::from(sb.agblocks) * bs;
    let read = |b: u32| -> fs_xfs::Result<Vec<u8>> {
        let mut raw = vec![0u8; bs as usize];
        dev.read_at(ag_start + u64::from(b) * bs, &mut raw)?;
        Ok(raw)
    };
    let free: u64 = walk_from_agf(&sb, &agf, Order::ByBlock, read)
        .expect("free space")
        .iter()
        .map(|e| u64::from(e.blockcount))
        .sum();
    let data = pattern((free * bs + 1024 * 1024) as usize);
    fs.write(ino, 0, &data)
        .expect("a write larger than one group");

    let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
    assert_eq!(inode.size, data.len() as u64);
    let extents = fs.data_extents(&inode, &raw).expect("extents");
    let groups: BTreeSet<u32> = extents
        .iter()
        .map(|e| sb.split_fsblock(e.startblock).0)
        .collect();
    assert!(
        groups.len() >= 2,
        "{} bytes, more than the root's group has free, landed in groups {groups:?}",
        data.len()
    );
    let got = fs.read_file(&inode, &raw).expect("read");
    assert!(got == data, "the file does not read back as written");
    assert_eq!(
        inode.nblocks,
        extents.iter().map(|e| e.blockcount).sum::<u64>(),
        "the block count is not what the extents map"
    );
}
