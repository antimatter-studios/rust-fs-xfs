//! A group's inode and free-space B+trees walked for salvage, on images
//! `mkfs.xfs` and the kernel made, and on damaged copies of them.
//!
//! Undamaged, the salvage walks have to return what the strict walkers do
//! (themselves checked against `xfs_db` in `alloc_btree_oracle.rs` and
//! `oracle_vm_fixtures.rs`, and here in `salvage_groups_oracle.rs`), and
//! scanning only the chunks the inode trees name has to find every live
//! inode a scan of the whole device finds. Damaged, a broken leaf is
//! reported where it was broken, and a whole-device scan still finds what
//! the tree lost.

mod common;

use common::{fixture, fixtures_matching, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, DamageKind, Extent, InodeState};
use fs_xfs::Filesystem;
use std::collections::BTreeSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const SUITE: &str = "salvage_groups";

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

fn images() -> Vec<PathBuf> {
    let mut out = fixtures_matching("xfs-", ".img");
    out.extend(fixtures_matching("xfsdata-", ".img"));
    assert!(!out.is_empty(), "no fixtures: `chore fixtures` builds them");
    out
}

fn live_by_device_scan(device: &FileDevice, image: &Path) -> BTreeSet<u64> {
    let sb = salvage::superblocks(device).geometry.unwrap();
    let length = std::fs::metadata(image).unwrap().len();
    let mut found = BTreeSet::new();
    salvage::find_inodes(device, &sb, Extent::new(0, length), &mut |c| {
        if c.state == InodeState::Live {
            found.insert(c.ino);
        }
        true
    });
    found
}

/// On every fixture and every group, the salvage walks return what the
/// strict walkers return, with no damage, and the chunks they name hold
/// every live inode on the device.
#[test]
fn undamaged_trees_match_the_strict_walkers() {
    for image in images() {
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        let device = open(&image);
        let fs = Filesystem::mount(device.clone()).unwrap();
        let sb = fs.superblock().clone();
        let mut in_chunks = BTreeSet::new();
        for ag in 0..sb.agcount {
            let headers = salvage::ag_headers(device.as_ref(), &sb, ag);
            let agi = headers.agi.value.unwrap();
            let agf = headers.agf.value.unwrap();

            let chunks = salvage::inode_chunks(device.as_ref(), &sb, ag, &agi);
            assert!(
                chunks.damage.is_empty(),
                "{name} AG {ag}: {:#?}",
                chunks.damage
            );
            let strict = fs_xfs::inode_btree::Trees::open(&sb, device.as_ref(), ag).unwrap();
            assert_eq!(chunks.records, strict.chunks(), "{name} AG {ag}");

            let free = salvage::free_extents(device.as_ref(), &sb, ag, &agf);
            assert!(free.damage.is_empty(), "{name} AG {ag}: {:#?}", free.damage);
            assert_eq!(free.records, fs.free_extents(ag).unwrap(), "{name} AG {ag}");

            salvage::find_inodes_in_chunks(device.as_ref(), &sb, ag, &chunks.records, &mut |c| {
                if c.state == InodeState::Live {
                    in_chunks.insert(c.ino);
                }
                true
            });
        }
        assert_eq!(
            in_chunks,
            live_by_device_scan(&device, &image),
            "{name}: the chunks hold every live inode"
        );
    }
}

fn invert(path: &Path, at: u64) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0u8];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.read_exact(&mut byte).unwrap();
    byte[0] = !byte[0];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&byte).unwrap();
    f.sync_all().unwrap();
}

/// An inode-tree leaf whose magic is broken is reported at its block and
/// its chunks are lost from the walk; the device scan still finds every
/// inode they held.
#[test]
fn a_broken_inode_tree_leaf_is_reported_and_the_device_scan_still_finds_its_inodes() {
    let image = fixture("xfsdata-default.img");
    let (sb, leaf, want) = {
        let device = open(&image);
        let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
        let agi = salvage::ag_headers(device.as_ref(), &sb, 0)
            .agi
            .value
            .unwrap();
        let walk = salvage::inode_chunks(device.as_ref(), &sb, 0, &agi);
        (
            sb,
            *walk.blocks.last().unwrap(),
            live_by_device_scan(&device, &image),
        )
    };
    let copy = scratch::Volume::copy_of(
        SUITE,
        &image,
        &format!("{}-inobt-magic.img", std::process::id()),
    );
    invert(copy.path(), leaf.offset);
    let device = open(copy.path());
    let agi = salvage::ag_headers(device.as_ref(), &sb, 0)
        .agi
        .value
        .unwrap();
    let walk = salvage::inode_chunks(device.as_ref(), &sb, 0, &agi);
    assert!(
        walk.damage
            .iter()
            .any(|d| d.kind == DamageKind::Signature && d.location == leaf),
        "{:#?}",
        walk.damage
    );
    assert!(!walk.blocks.contains(&leaf));
    assert_eq!(live_by_device_scan(&device, copy.path()), want);
}

/// A free-space leaf that fails only its checksum is reported and still
/// used: its records are all there.
#[test]
fn a_free_space_leaf_that_fails_its_checksum_is_still_used() {
    let image = fixture("xfs-default.img");
    let (sb, leaf, want) = {
        let device = open(&image);
        let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
        let agf = salvage::ag_headers(device.as_ref(), &sb, 1)
            .agf
            .value
            .unwrap();
        let walk = salvage::free_extents(device.as_ref(), &sb, 1, &agf);
        (sb, *walk.blocks.last().unwrap(), walk.records)
    };
    let copy = scratch::Volume::copy_of(
        SUITE,
        &image,
        &format!("{}-bnobt-crc.img", std::process::id()),
    );
    // The block's last byte: past its records, covered by the checksum.
    invert(copy.path(), leaf.offset + leaf.length - 1);
    let device = open(copy.path());
    let agf = salvage::ag_headers(device.as_ref(), &sb, 1)
        .agf
        .value
        .unwrap();
    let walk = salvage::free_extents(device.as_ref(), &sb, 1, &agf);
    assert_eq!(walk.records, want);
    assert_eq!(walk.damage.len(), 1, "{:#?}", walk.damage);
    assert_eq!(walk.damage[0].kind, DamageKind::Checksum);
    assert_eq!(walk.damage[0].location, leaf);
}
