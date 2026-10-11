//! A directory data block that does not parse fails the listing, rather
//! than vanishing from it (#438).
//!
//! XFS leaves a directory's free space unmapped: a data block that empties
//! is given back, and an empty one still parses, as a header and one free
//! record. So every mapped block in the data region holds entries, and one
//! that does not parse is damage. `read_dir` skipped it as free space, so
//! its names were silently missing from the listing while a lookup through
//! the hash index could still find them.
//!
//! Each case grows a directory, settles it to disk (a sync, then one more
//! record, so a fresh mount replays only that), damages the first data
//! block's `.` entry tag with the block's checksum stamped again, and
//! lists it on a fresh read-only mount. The checker reports the same
//! damage as `dir.unreadable`, which `tests/cli_fsck_oracle.rs` grades
//! against `xfs_repair -n`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
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

/// The v5 data block header: magic, CRC, address, LSN, UUID, owner, the
/// three best-free records and padding.
const DATA_HDR: usize = 64;
/// The `.` entry: inode number, name length, one byte of name, file type
/// and tag, aligned to 8. Its tag is its last two bytes.
const DOT_TAG: usize = DATA_HDR + 14;

/// A directory `d` of `names` hard links, settled to disk, its first data
/// block's `.` tag damaged and the block's CRC stamped again.
fn damaged(names: usize) -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let (file, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    for i in 0..names {
        fs.link(file, dir, format!("name-{i:04}").as_bytes())
            .expect("link");
    }
    fs.sync().expect("sync");
    fs.create_file(root, b"settled", 0o100644)
        .expect("a record after the push");

    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    let first = fs
        .data_extents(&inode, &raw)
        .expect("extents")
        .into_iter()
        .find(|e| e.startoff == 0)
        .expect("a first data block");
    let at = fs.superblock().fsblock_offset(first.startblock);
    let mut block = vec![0u8; fs.superblock().dirblocksize() as usize];
    fs.device().read_at(at, &mut block).expect("read");
    drop(fs);
    assert_eq!(
        u16::from_be_bytes([block[DOT_TAG], block[DOT_TAG + 1]]),
        DATA_HDR as u16,
        "the first entry is not `.` where it should be"
    );
    block[DOT_TAG..DOT_TAG + 2].copy_from_slice(&0u16.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut block, 4);
    dev.write_at(at, &block).expect("write");
    dev
}

fn listed(dev: &Arc<Sparse>) -> fs_xfs::Result<usize> {
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount");
    fs.list_path("/d").map(|e| e.len())
}

#[test]
fn a_block_form_directory_whose_block_does_not_parse_fails_the_listing() {
    let got = listed(&damaged(20));
    assert!(
        got.is_err(),
        "the damaged block was listed as {got:?} names"
    );
}

#[test]
fn a_leaf_form_directory_whose_data_block_does_not_parse_fails_the_listing() {
    let got = listed(&damaged(300));
    assert!(
        got.is_err(),
        "the damaged block was skipped: {got:?} names listed"
    );
}
