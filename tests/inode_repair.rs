//! Inode allocation and link counts are repaired by the planner's
//! inode-allocation rule, through `fsck.xfs -y` and through
//! `fs_xfs::repair` (#393). These formatter-built images prove the
//! contract; the independent kernel and xfsprogs verdicts live in
//! inode_repair_oracle.rs.

mod cli_support;

use cli_support::{scratch_dir, stderr, tool};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::{inode::offsets, Filesystem};
use std::sync::Arc;

/// Plan a repair of `dev` and apply it: how many changes, or why not.
fn repair(dev: &Arc<FileDevice>) -> Result<usize, String> {
    let fs = Filesystem::mount(dev.clone()).map_err(|e| e.to_string())?;
    let access = fs_xfs::repair::Exclusive::asserted_by_caller();
    let plan = fs_xfs::repair::plan(&fs, &access);
    if plan.status != fs_xfs::repair::Status::Ready {
        let found: Vec<(&str, String)> = plan
            .check
            .findings
            .iter()
            .map(|f| (f.code.as_str(), f.what.clone()))
            .collect();
        return Err(format!("{:?} after {found:?}", plan.refusals));
    }
    fs_xfs::repair::apply(&plan, dev.as_ref(), 0, &access).map_err(|e| e.to_string())
}

fn volume(tag: &str) -> (std::path::PathBuf, Arc<FileDevice>) {
    let path = scratch_dir(&format!("inode-repair-{tag}")).join("volume.img");
    std::fs::File::create(&path)
        .unwrap()
        .set_len(400 << 20)
        .unwrap();
    let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
    fs_xfs::mkfs::format(dev.as_ref(), &Default::default()).unwrap();
    (path, dev)
}

#[test]
fn a_wrong_root_link_count_is_repaired_and_a_second_run_changes_nothing() {
    let (path, dev) = volume("links");
    let fs = Filesystem::mount(dev.clone()).unwrap();
    let root = fs.superblock().rootino;
    let (_, mut raw) = fs.read_inode_raw(root).unwrap();
    raw[offsets::NLINK..offsets::NLINK + 4].copy_from_slice(&9u32.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut raw, offsets::CRC);
    dev.write_at(fs.inode_offset(root).unwrap(), &raw).unwrap();
    dev.flush().unwrap();
    assert!(!fs_xfs::check::check(&Filesystem::mount(dev.clone()).unwrap()).is_clean());

    let out = tool("fsck.xfs").arg("-y").arg(&path).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    let fixed = Filesystem::mount(dev.clone()).unwrap();
    assert_eq!(fixed.read_inode(root).unwrap().nlink, 2);
    assert!(fs_xfs::check::check(&fixed).is_clean());
    let first = fixed.read_inode_raw(root).unwrap().1;
    let out = tool("fsck.xfs").arg("-y").arg(&path).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        Filesystem::mount(dev)
            .unwrap()
            .read_inode_raw(root)
            .unwrap()
            .1,
        first
    );
}

#[test]
fn allocation_and_finobt_membership_are_reconstructed_from_verified_slots() {
    for fault in [
        "root-bit",
        "unused-bit",
        "inobt-count",
        "finobt-count",
        "finobt-missing",
    ] {
        let (_, dev) = volume(fault);
        let fs = Filesystem::mount(dev.clone()).unwrap();
        let sb = fs.superblock();
        let agi = fs.read_agi(0).unwrap();
        assert_eq!(agi.level, 1);
        let tree = if fault.starts_with("finobt") {
            agi.free_root
        } else {
            agi.root
        };
        let at = u64::from(tree) * u64::from(sb.blocksize);
        let mut raw = vec![0; sb.blocksize as usize];
        dev.read_at(at, &mut raw).unwrap();
        let record = fs_xfs::ag_btree::V5_HEADER_LEN;
        match fault {
            "root-bit" | "unused-bit" => {
                let start = u32::from_be_bytes(raw[record..record + 4].try_into().unwrap());
                let bit = if fault == "root-bit" {
                    sb.rootino - u64::from(start)
                } else {
                    63
                };
                let free = u64::from_be_bytes(raw[record + 8..record + 16].try_into().unwrap());
                raw[record + 8..record + 16].copy_from_slice(&(free ^ (1u64 << bit)).to_be_bytes());
            }
            "finobt-missing" => raw[6..8].copy_from_slice(&0u16.to_be_bytes()),
            _ => {
                if sb.has_sparse_inodes() {
                    raw[record + 7] = 0;
                } else {
                    raw[record + 4..record + 8].fill(0);
                }
            }
        }
        fs_xfs::group_write::restamp_crc(&mut raw, fs_xfs::ag_btree::offsets::CRC);
        dev.write_at(at, &raw).unwrap();
        assert!(
            !fs_xfs::check::check(&Filesystem::mount(dev.clone()).unwrap()).is_clean(),
            "{fault}"
        );
        let changed = repair(&dev).unwrap_or_else(|e| panic!("{fault}: {e}"));
        assert!(changed > 0, "{fault}");
        assert!(
            fs_xfs::check::check(&Filesystem::mount(dev.clone()).unwrap()).is_clean(),
            "{fault}"
        );
        assert_eq!(repair(&dev), Ok(0), "{fault}: a second repair");
    }
}

#[test]
fn an_unused_slot_that_still_claims_blocks_refuses_link_repairs_unchanged() {
    let (_, dev) = volume("uncertain-owner");
    let fs = Filesystem::mount(dev.clone()).unwrap();
    let root = fs.superblock().rootino;
    let root_at = fs.inode_offset(root).unwrap();
    let (_, mut raw) = fs.read_inode_raw(root).unwrap();
    raw[offsets::NLINK..offsets::NLINK + 4].copy_from_slice(&9u32.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut raw, offsets::CRC);
    dev.write_at(root_at, &raw).unwrap();
    let bad_at = fs.inode_offset(root + 63).unwrap();
    let (_, mut unused) = fs.read_inode_raw(root + 63).unwrap();
    unused[offsets::NBLOCKS..offsets::NBLOCKS + 8].copy_from_slice(&1u64.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut unused, offsets::CRC);
    dev.write_at(bad_at, &unused).unwrap();
    assert!(repair(&dev).is_err());
    let mut root_after = vec![0; raw.len()];
    let mut unused_after = vec![0; unused.len()];
    dev.read_at(root_at, &mut root_after).unwrap();
    dev.read_at(bad_at, &mut unused_after).unwrap();
    assert_eq!(root_after, raw);
    assert_eq!(unused_after, unused);
}

#[test]
fn explicit_repair_honours_a_partition_offset_and_preserves_its_prefix() {
    let path = scratch_dir("inode-repair-offset").join("disk.img");
    let offset = 1 << 20;
    let size = 400 << 20;
    std::fs::File::create(&path)
        .unwrap()
        .set_len(offset + size)
        .unwrap();
    let disk = Arc::new(FileDevice::open_rw(&path).unwrap());
    disk.write_at(0, b"partition prefix").unwrap();
    let part = Arc::new(fs_core::OwnedRwSlice::new(disk.clone(), offset, size));
    fs_xfs::mkfs::format(part.as_ref(), &Default::default()).unwrap();
    let fs = Filesystem::mount(part.clone()).unwrap();
    let root = fs.superblock().rootino;
    let (_, mut raw) = fs.read_inode_raw(root).unwrap();
    raw[offsets::NLINK..offsets::NLINK + 4].copy_from_slice(&9u32.to_be_bytes());
    fs_xfs::group_write::restamp_crc(&mut raw, offsets::CRC);
    part.write_at(fs.inode_offset(root).unwrap(), &raw).unwrap();
    let out = tool("fsck.xfs")
        .args(["-y", "--offset", &offset.to_string()])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert_eq!(
        Filesystem::mount(part)
            .unwrap()
            .read_inode(root)
            .unwrap()
            .nlink,
        2
    );
    let mut prefix = [0; 16];
    disk.read_at(0, &mut prefix).unwrap();
    assert_eq!(&prefix, b"partition prefix");
}
