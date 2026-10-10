//! A damaged secondary superblock is rewritten from the primary, and only
//! that copy (#391).
//!
//! Each case damages a copy on a volume this crate's `mkfs` made in
//! memory, plans a repair, applies it and checks the volume again: one
//! field, or the checksum alone, is repaired; a copy naming another
//! filesystem, and two damaged copies, refuse the plan, and nothing is
//! written. What `xfs_db` and `xfs_repair -n` make of a repair is
//! `tests/cli_superblock_repair_oracle.rs`.

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

const SECT: usize = 512;
const LOGBLOCKS: usize = 96;
const UUID: usize = 32;
const CRC: usize = 224;

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

/// Where group `ag`'s superblock copy starts.
fn copy_at(dev: &Arc<Sparse>, ag: u64) -> u64 {
    let sb = mount(dev).superblock().clone();
    ag * u64::from(sb.agblocks) * u64::from(sb.blocksize)
}

/// Change group `ag`'s copy with `edit`, and stamp its checksum again
/// when `restamp` says so.
fn damage(dev: &Arc<Sparse>, ag: u64, restamp: bool, edit: impl Fn(&mut [u8])) {
    let at = copy_at(dev, ag);
    let mut sector = vec![0u8; SECT];
    dev.read_at(at, &mut sector).unwrap();
    edit(&mut sector);
    if restamp {
        fs_xfs::group_write::restamp_crc(&mut sector, CRC);
    }
    dev.write_at(at, &sector).unwrap();
}

fn image(dev: &Arc<Sparse>) -> BTreeMap<u64, Box<[u8; SECTOR]>> {
    dev.0.lock().unwrap().clone()
}

/// The volume changed in group `ag`'s copy and nowhere else, and that
/// copy is now the primary, byte for byte: what `xfs_repair` writes.
fn only_the_copy_changed(dev: &Arc<Sparse>, before: &BTreeMap<u64, Box<[u8; SECTOR]>>, ag: u64) {
    let sector = copy_at(dev, ag) / SECT as u64;
    let mut after = image(dev);
    let mut before = before.clone();
    let copy = after.remove(&sector).expect("the copy");
    before.remove(&sector);
    assert!(
        after == before,
        "the repair changed more than group {ag}'s copy"
    );
    assert_eq!(
        copy.as_slice(),
        image(dev)[&0].as_slice(),
        "the copy is not the primary"
    );
}

/// Plan and apply, then check again: how many changes, and what is left.
fn repair(dev: &Arc<Sparse>) -> (usize, Vec<Code>) {
    let fs = mount(dev);
    let access = Exclusive::asserted_by_caller();
    let planned = plan(&fs, &access);
    assert_eq!(planned.status, Status::Ready, "{:?}", planned.refusals);
    let n = apply(&planned, dev.as_ref(), 0, &access).expect("apply");
    let left = fs_xfs::check::check(&mount(dev));
    (n, left.findings.iter().map(|f| f.code).collect())
}

#[test]
fn one_damaged_field_is_rewritten_from_the_primary_and_only_that() {
    let dev = volume();
    damage(&dev, 1, true, |s| {
        s[LOGBLOCKS..LOGBLOCKS + 4].copy_from_slice(&1234u32.to_be_bytes())
    });
    let damaged = image(&dev);
    let codes: Vec<Code> = fs_xfs::check::check(&mount(&dev))
        .findings
        .iter()
        .map(|f| f.code)
        .collect();
    assert_eq!(codes, vec![Code::SbCopyField]);
    assert_eq!(repair(&dev), (1, vec![]));
    only_the_copy_changed(&dev, &damaged, 1);
    // Again: nothing to do, nothing written.
    let repaired = image(&dev);
    assert_eq!(repair(&dev), (0, vec![]));
    assert!(image(&dev) == repaired);
}

#[test]
fn a_copy_that_fails_its_checksum_is_rewritten_and_the_scan_was_complete() {
    let dev = volume();
    damage(&dev, 2, false, |s| s[LOGBLOCKS + 3] ^= 0x10);
    let damaged = image(&dev);
    let report = fs_xfs::check::check(&mount(&dev));
    let codes: Vec<Code> = report.findings.iter().map(|f| f.code).collect();
    assert_eq!(codes, vec![Code::SbCopyUnreadable]);
    assert_eq!(
        report.scan.as_str(),
        "complete",
        "nothing lies under a copy"
    );
    assert_eq!(repair(&dev), (1, vec![]));
    only_the_copy_changed(&dev, &damaged, 2);
}

#[test]
fn a_copy_naming_another_filesystem_or_two_damaged_copies_refuse_and_write_nothing() {
    let other = volume();
    damage(&other, 1, true, |s| s[UUID] ^= 0xff);
    let two = volume();
    damage(&two, 1, true, |s| {
        s[LOGBLOCKS..LOGBLOCKS + 4].copy_from_slice(&1234u32.to_be_bytes())
    });
    damage(&two, 3, false, |s| s[LOGBLOCKS + 3] ^= 0x10);
    for dev in [other, two] {
        let before = image(&dev);
        let fs = mount(&dev);
        let access = Exclusive::asserted_by_caller();
        let planned = plan(&fs, &access);
        assert_eq!(planned.status, Status::Refused, "{:?}", planned.changes);
        assert!(planned.changes.is_empty());
        assert!(apply(&planned, dev.as_ref(), 0, &access).is_err());
        assert!(image(&dev) == before, "a refused plan wrote");
    }
}

#[test]
fn a_plan_made_from_bytes_that_have_changed_since_is_not_applied() {
    let dev = volume();
    damage(&dev, 1, true, |s| {
        s[LOGBLOCKS..LOGBLOCKS + 4].copy_from_slice(&1234u32.to_be_bytes())
    });
    let fs = mount(&dev);
    let access = Exclusive::asserted_by_caller();
    let planned = plan(&fs, &access);
    damage(&dev, 1, true, |s| {
        s[LOGBLOCKS..LOGBLOCKS + 4].copy_from_slice(&999u32.to_be_bytes())
    });
    let before = image(&dev);
    assert!(apply(&planned, dev.as_ref(), 0, &access).is_err());
    assert!(image(&dev) == before, "a stale plan wrote");
}
