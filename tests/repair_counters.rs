//! Group and superblock counters are counted again from the trees they
//! summarise, and nothing else changes (#392).
//!
//! Each case damages one counter on a volume this crate's `mkfs` made in
//! memory, its header's checksum stamped again, then plans and applies a
//! repair: the counter must read as it did before the damage, byte for
//! byte, and a second plan must find nothing. A counter beside a tree the
//! check did not find sound refuses the plan and writes nothing. What
//! `xfs_repair -n`, `xfs_db` and the kernel make of a repair is
//! `tests/counter_repair_kernel.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::check::Code;
use fs_xfs::repair::{apply, plan, Exclusive, Status};
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

const SECT: u64 = 512;

fn volume() -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    let options = fs_xfs::mkfs::Options {
        agcount: Some(4),
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    dev
}

fn mount(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount")
}

fn image(dev: &Arc<Sparse>) -> BTreeMap<u64, Box<[u8; SECTOR]>> {
    dev.0.lock().unwrap().clone()
}

/// Write `value` over `width` bytes at `field` of the sector at `at`, and
/// stamp the checksum at `crc` again.
fn damage(dev: &Arc<Sparse>, at: u64, field: usize, width: usize, value: u64, crc: usize) {
    let mut sector = vec![0u8; SECT as usize];
    dev.read_at(at, &mut sector).unwrap();
    sector[field..field + width].copy_from_slice(&value.to_be_bytes()[8 - width..]);
    fs_xfs::group_write::restamp_crc(&mut sector, crc);
    dev.write_at(at, &sector).unwrap();
}

fn codes(dev: &Arc<Sparse>) -> Vec<Code> {
    fs_xfs::check::check(&mount(dev))
        .findings
        .iter()
        .map(|f| f.code)
        .collect()
}

#[test]
fn each_counter_is_counted_again_and_nothing_else_changes() {
    use fs_xfs::ag::offsets::{agf, agi};
    use fs_xfs::superblock::offsets as so;
    let group = |dev: &Arc<Sparse>, ag: u64| {
        let sb = mount(dev).superblock().clone();
        ag * u64::from(sb.agblocks) * u64::from(sb.blocksize)
    };
    let cases: Vec<(&str, u64, u64, usize, usize, u64, usize)> = vec![
        // (name, group, sector in it, field, width, bad value, crc)
        ("agf freeblks", 1, 1, agf::FREEBLKS, 4, 1, agf::CRC),
        ("agf longest", 2, 1, agf::LONGEST, 4, 7, agf::CRC),
        ("agf btreeblks", 0, 1, agf::BTREEBLKS, 4, 99, agf::CRC),
        ("agi count", 0, 2, agi::COUNT, 4, 1024, agi::CRC),
        ("agi freecount", 0, 2, agi::FREECOUNT, 4, 5, agi::CRC),
        ("sb icount", 0, 0, so::ICOUNT, 8, 99_999, so::CRC),
        ("sb ifree", 0, 0, so::IFREE, 8, 99_999, so::CRC),
        ("sb fdblocks", 0, 0, so::FDBLOCKS, 8, 1, so::CRC),
    ];
    for (name, ag, sector, field, width, bad, crc) in cases {
        let dev = volume();
        assert!(
            codes(&dev).is_empty(),
            "{name}: the fresh volume is not clean"
        );
        let clean = image(&dev);
        let at = group(&dev, ag) + sector * SECT;
        damage(&dev, at, field, width, bad, crc);
        let found = codes(&dev);
        assert!(!found.is_empty(), "{name}: no damage made");
        assert!(image(&dev) != clean);

        let fs = mount(&dev);
        let access = Exclusive::asserted_by_caller();
        let planned = plan(&fs, &access);
        assert_eq!(
            planned.status,
            Status::Ready,
            "{name}: {found:?} {:?}",
            planned.refusals
        );
        apply(&planned, dev.as_ref(), 0, &access).expect("apply");
        assert!(codes(&dev).is_empty(), "{name}: {:?} left", codes(&dev));
        assert!(
            image(&dev) == clean,
            "{name}: not the bytes from before the damage"
        );
        let again = plan(&mount(&dev), &access);
        assert!(
            again.changes.is_empty(),
            "{name}: a second plan changes {:?}",
            again.changes
        );
    }
}

#[test]
fn a_counter_beside_an_unsound_tree_refuses_the_plan_and_writes_nothing() {
    use fs_xfs::ag::offsets::agf;
    let dev = volume();
    let sb = mount(&dev).superblock().clone();
    let ag_start = u64::from(sb.agblocks) * u64::from(sb.blocksize);
    damage(&dev, ag_start + SECT, agf::FREEBLKS, 4, 1, agf::CRC);
    // The by-count tree's only record shortened, so the two free-space
    // trees disagree about what is free.
    let mut agf_raw = vec![0u8; SECT as usize];
    dev.read_at(ag_start + SECT, &mut agf_raw).unwrap();
    let at = agf::ROOTS + 4;
    let cnt_root = u64::from(u32::from_be_bytes(agf_raw[at..at + 4].try_into().unwrap()));
    let bs = u64::from(sb.blocksize);
    let mut block = vec![0u8; bs as usize];
    dev.read_at(ag_start + cnt_root * bs, &mut block).unwrap();
    let count = fs_xfs::ag_btree::V5_HEADER_LEN + 4;
    let n = u32::from_be_bytes(block[count..count + 4].try_into().unwrap());
    block[count..count + 4].copy_from_slice(&(n - 1).to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut block, fs_xfs::ag_btree::offsets::CRC);
    dev.write_at(ag_start + cnt_root * bs, &block).unwrap();
    assert!(
        codes(&dev).contains(&Code::FreespDisagree),
        "{:?}",
        codes(&dev)
    );

    let before = image(&dev);
    let fs = mount(&dev);
    let access = Exclusive::asserted_by_caller();
    let planned = plan(&fs, &access);
    assert_eq!(planned.status, Status::Refused);
    assert!(planned.changes.is_empty());
    assert!(apply(&planned, dev.as_ref(), 0, &access).is_err());
    assert!(image(&dev) == before, "a refused plan wrote");
}
