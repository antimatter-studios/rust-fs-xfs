//! Two files exchange ranges of their data by exchanging the blocks that
//! map them, not by copying (#374).
//!
//! The volume is this crate's `mkfs` with the exchange-range incompat bit
//! set in every superblock copy, which is what `mkfs.xfs -i exchange=1`
//! adds: the bit changes no structure a write here keeps, and the kernel
//! refuses the exchange on a volume without it. Each case checks from the
//! outside: what each file reads, how many blocks it holds, that a fresh
//! mount reads the same through the log, and that the checker finds
//! nothing wrong. What `xfs_repair -n` and `xfs_db` make of an exchange on
//! a volume with a reverse-mapping tree is `tests/exchange_range_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::superblock::{incompat, offsets as sbo};
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;
const BS: u64 = 4096;

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

/// A volume from this crate's `mkfs`, with `bits` added to the incompat
/// features of every superblock copy, each copy's checksum stamped again.
fn volume(bits: u32) -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let sb = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>)
        .expect("mount")
        .superblock()
        .clone();
    for ag in 0..u64::from(sb.agcount) {
        let at = ag * u64::from(sb.agblocks) * u64::from(sb.blocksize);
        let mut sector = vec![0u8; SECTOR];
        dev.read_at(at, &mut sector).unwrap();
        let was = u32::from_be_bytes(
            sector[sbo::FEATURES_INCOMPAT..sbo::FEATURES_INCOMPAT + 4]
                .try_into()
                .unwrap(),
        );
        sector[sbo::FEATURES_INCOMPAT..sbo::FEATURES_INCOMPAT + 4]
            .copy_from_slice(&(was | bits).to_be_bytes());
        fs_xfs::group_write::restamp_crc(&mut sector, sbo::CRC);
        dev.write_at(at, &sector).unwrap();
    }
    dev
}

fn mount_rw(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw")
}

/// A fresh read-only mount, which replays what the writing one logged.
fn replayed(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("remount")
}

fn image(dev: &Arc<Sparse>) -> BTreeMap<u64, Box<[u8; SECTOR]>> {
    dev.0.lock().unwrap().clone()
}

/// `n` blocks, each filled with `fill` plus its block number.
fn blocks(fill: u8, n: u64) -> Vec<u8> {
    (0..n)
        .flat_map(|i| vec![fill.wrapping_add(i as u8); BS as usize])
        .collect()
}

fn contents(fs: &Filesystem, ino: u64) -> (Vec<u8>, u64) {
    let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
    (fs.read_file(&inode, &raw).expect("read"), inode.nblocks)
}

/// `a` holds three blocks; `b` four, the third a hole.
fn two_files(fs: &Filesystem) -> (u64, u64) {
    let root = fs.superblock().rootino;
    let (a, _) = fs.create_file(root, b"a", 0o100644).expect("create a");
    let (b, _) = fs.create_file(root, b"b", 0o100644).expect("create b");
    fs.write(a, 0, &blocks(0xa0, 3)).expect("write a");
    fs.write(b, 0, &blocks(0xb0, 2)).expect("write b");
    fs.write(b, 3 * BS, &blocks(0xb3, 1))
        .expect("write b's last block");
    (a, b)
}

#[test]
fn two_files_exchange_their_blocks_and_each_reads_the_others_data() {
    let dev = volume(incompat::EXCHRANGE);
    let fs = mount_rw(&dev);
    let (a, b) = two_files(&fs);
    let before_a = contents(&fs, a);
    let before_b = contents(&fs, b);
    assert_eq!((before_a.1, before_b.1), (3, 3));
    fs.exchange_range(a, 0, b, 0, 3 * BS).expect("exchange");

    // What each file should read: the other's first three blocks, the
    // hole coming across as zeros, and `b` keeping its own fourth block.
    let mut want_a = before_b.0[..3 * BS as usize].to_vec();
    want_a[2 * BS as usize..].fill(0);
    let mut want_b = before_a.0.clone();
    want_b.extend_from_slice(&before_b.0[3 * BS as usize..]);

    // The mount that made the exchange, and a fresh one that has to find
    // it through the log.
    let fresh = replayed(&dev);
    for fs in [&fs, &fresh] {
        let (got_a, held_a) = contents(fs, a);
        let (got_b, held_b) = contents(fs, b);
        assert!(got_a == want_a, "a reads other than b's first blocks");
        assert!(
            got_b == want_b,
            "b reads other than a's blocks and its own last one"
        );
        assert_eq!(
            (held_a, held_b),
            (2, 4),
            "the blocks went with the mappings"
        );
    }
    let report = fs_xfs::check::check(&fresh);
    assert!(report.is_clean(), "{:#?}", report.findings);
}

#[test]
fn ranges_at_different_offsets_exchange_one_block_each() {
    let dev = volume(incompat::EXCHRANGE);
    let fs = mount_rw(&dev);
    let (a, b) = two_files(&fs);
    let (old_a, _) = contents(&fs, a);
    let (old_b, _) = contents(&fs, b);
    fs.exchange_range(a, BS, b, 3 * BS, BS).expect("exchange");
    let (got_a, _) = contents(&fs, a);
    let (got_b, _) = contents(&fs, b);
    let bs = BS as usize;
    assert!(
        got_a[bs..2 * bs] == old_b[3 * bs..],
        "a's second block is b's last"
    );
    assert!(
        got_b[3 * bs..] == old_a[bs..2 * bs],
        "b's last block is a's second"
    );
    assert!(got_a[..bs] == old_a[..bs] && got_a[2 * bs..] == old_a[2 * bs..]);
    assert!(got_b[..3 * bs] == old_b[..3 * bs]);
    let fresh = replayed(&dev);
    assert!(contents(&fresh, a).0 == got_a && contents(&fresh, b).0 == got_b);
    let report = fs_xfs::check::check(&fresh);
    assert!(report.is_clean(), "{:#?}", report.findings);
}

#[test]
fn an_exchange_refused_writes_nothing() {
    // No exchange-range bit: the kernel refuses, and so does this.
    let plain = volume(0);
    {
        let fs = mount_rw(&plain);
        let (a, b) = two_files(&fs);
        let before = image(&plain);
        let refused = fs.exchange_range(a, 0, b, 0, BS);
        assert!(
            matches!(refused, Err(Error::UnsupportedFeature(_))),
            "{refused:?}"
        );
        assert!(image(&plain) == before, "a refused exchange wrote");
    }

    let dev = volume(incompat::EXCHRANGE);
    let fs = mount_rw(&dev);
    let (a, b) = two_files(&fs);
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let before = image(&dev);
    for (what, result) in [
        ("an unaligned offset", fs.exchange_range(a, 1, b, 0, BS)),
        ("an unaligned length", fs.exchange_range(a, 0, b, 0, BS + 1)),
        (
            "a range past a's end",
            fs.exchange_range(a, 0, b, 0, 4 * BS),
        ),
        ("one inode with itself", fs.exchange_range(a, 0, a, BS, BS)),
        ("a directory", fs.exchange_range(a, 0, dir, 0, BS)),
        ("no length", fs.exchange_range(a, 0, b, 0, 0)),
    ] {
        assert!(result.is_err(), "{what} was exchanged: {result:?}");
        assert!(image(&dev) == before, "{what}: a refused exchange wrote");
    }
}
