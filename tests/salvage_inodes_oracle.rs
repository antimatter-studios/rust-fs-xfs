//! The salvage extent walk against `xfs_db`'s `bmap`, and its damage
//! findings against `xfs_repair -n`, in the harness guest.
//!
//! `bmap -d` walks a file's data fork with xfsprogs' own code and prints
//! each extent as its file block, its packed start block and its length.
//! The salvage walk of the same inode has to place the same blocks at the
//! same file offsets. Damage made by `xfs_db -x` in the file's map has to
//! be found by the walk, and called damage by `xfs_repair -n`.

mod common;

use common::{fixture, oracle, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, DamageKind, FileExtent, WalkEnd};
use fs_xfs::{Filesystem, Superblock};
use std::path::Path;
use std::sync::Arc;

const SUITE: &str = "salvage_inodes_oracle";

const FILES: &[&str] = &["/fragmented.bin", "/large.bin", "/medium.bin", "/small.txt"];

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

/// `(file block, start block, count)` for each line `bmap -d` printed:
/// `data offset 0 startblock 24 (0/24) count 1 flag 0`.
fn bmap(text: &str) -> Vec<(u64, u64, u64)> {
    text.lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let after = |key: &str| {
                let at = words.iter().position(|w| *w == key)?;
                words.get(at + 1)?.parse::<u64>().ok()
            };
            Some((after("offset")?, after("startblock")?, after("count")?))
        })
        .collect()
}

/// What the salvage walk must say, from `xfs_db`'s extents: each one placed
/// at its device offset and trimmed to the file's size.
fn expected(sb: &Superblock, extents: &[(u64, u64, u64)], size: u64) -> Vec<(u64, u64, u64)> {
    let block = u64::from(sb.blocksize);
    extents
        .iter()
        .filter(|(offset, _, _)| offset * block < size)
        .map(|&(offset, start, count)| {
            let file_offset = offset * block;
            (
                file_offset,
                sb.fsblock_offset(start),
                (count * block).min(size - file_offset),
            )
        })
        .collect()
}

fn walked(extents: &[FileExtent]) -> Vec<(u64, u64, u64)> {
    extents
        .iter()
        .map(|e| (e.file_offset, e.location.offset, e.location.length))
        .collect()
}

/// Every block-mapped file of the default data fixture is placed where
/// `xfs_db` maps it.
#[test]
fn extent_walks_agree_with_xfs_db_bmap() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
    for path in FILES {
        let ino = fs.lookup_path(path).unwrap().ino;
        let out = oracle("xfs_db")
            .args(["-r", "-c", &format!("inode {ino}"), "-c", "bmap -d"])
            .arg(image.to_str().unwrap())
            .output();
        assert!(
            out.ok(),
            "{path}: xfs_db failed:\n{}{}",
            out.stdout,
            out.stderr
        );
        let theirs = bmap(&out.stdout);
        assert!(
            !theirs.is_empty(),
            "{path}: xfs_db mapped nothing:\n{}",
            out.stdout
        );
        let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
        let map = salvage::walk_extents(device.as_ref(), &sb, &candidate);
        assert_eq!(map.end, WalkEnd::Complete, "{path}: {:#?}", map.damage);
        assert_eq!(
            walked(&map.extents),
            expected(&sb, &theirs, map.claimed),
            "{path}:\n{}",
            out.stdout
        );
    }
}

/// A leaf of a file's map whose magic `xfs_db` breaks is found by the walk
/// at the block `xfs_db` wrote, and `xfs_repair -n` calls the copy damaged.
#[test]
fn a_map_block_xfs_db_breaks_is_found_and_repair_agrees() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
    let ino = fs.lookup_path("/fragmented.bin").unwrap().ino;
    let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
    let clean = salvage::walk_extents(device.as_ref(), &sb, &candidate);
    let leaf = *clean.map_blocks.last().expect("a B+tree-mapped file");
    let fsblock = {
        // The leaf's packed block number, from its device offset.
        let block = u64::from(sb.blocksize);
        let group = u64::from(sb.agblocks) * block;
        let ag = leaf.offset / group;
        (ag << sb.agblklog) | ((leaf.offset % group) / block)
    };

    let copy = scratch::Volume::copy_of(
        SUITE,
        &image,
        &format!("{}-bmbt-magic.img", std::process::id()),
    );
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            &format!("fsblock {fsblock}"),
            "-c",
            "type bmapbtd",
            "-c",
            "write -d magic 0",
        ])
        .arg(copy.path().to_str().unwrap())
        .output();
    assert!(
        edit.ok(),
        "xfs_db could not break the leaf:\n{}{}",
        edit.stdout,
        edit.stderr
    );

    let damaged = open(copy.path());
    let candidate = salvage::read_inode(damaged.as_ref(), &sb, ino).unwrap();
    let map = salvage::walk_extents(damaged.as_ref(), &sb, &candidate);
    assert_eq!(map.end, WalkEnd::Broken);
    assert!(
        map.damage
            .iter()
            .any(|d| d.kind == DamageKind::Signature && d.location == leaf),
        "{:#?}",
        map.damage
    );

    let report = oracle("xfs_repair")
        .args(["-n", copy.path().to_str().unwrap()])
        .output()
        .repair_report();
    assert!(
        !report.contains(common::repair::IGNORED_THE_LOG),
        "xfs_repair ignored the log (\"valuable metadata changes in a log\"), so its report says nothing about this copy:\n{report}"
    );
    assert!(
        report.contains("REPAIR_RC=") && !report.contains("REPAIR_RC=0"),
        "xfs_repair -n found nothing wrong:\n{report}"
    );
    assert!(
        report.contains(&ino.to_string()),
        "xfs_repair -n does not name inode {ino}:\n{report}"
    );
}
