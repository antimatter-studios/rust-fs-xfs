//! A truncated image: the salvage reads keep everything before the cut,
//! and say exactly what was lost after it.
//!
//! The image is the default data fixture, which the kernel wrote, cut in the
//! middle of `/large.bin`'s data. Every file whose map and data lie before
//! the cut has to read back to the SHA-256 Linux computed, and every other
//! file has to say what it lost; `/large.bin` has to read
//! back byte for byte up to the cut, with the rest listed as unreadable;
//! superblock copies and group headers past the cut are reported unreadable,
//! and the geometry still comes from the primary.

mod common;

use common::{fixture, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, DamageKind, Extent, ExtentMap, WalkEnd};
use fs_xfs::{Filesystem, Superblock};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::Arc;

const SUITE: &str = "salvage_truncated";

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

fn map_of(device: &FileDevice, sb: &Superblock, ino: u64) -> ExtentMap {
    let candidate = salvage::read_inode(device, sb, ino).expect("the inode is before the cut");
    salvage::walk_extents(device, sb, &candidate)
}

fn read_all(device: &FileDevice, map: &ExtentMap) -> (Vec<u8>, Vec<Extent>) {
    let size = map.inline.map_or(map.claimed, |inline| inline.length);
    let mut bytes = vec![0u8; size as usize];
    let (_, unreadable) = salvage::read_mapped(device, map, 0, &mut bytes);
    (bytes, unreadable)
}

#[test]
fn a_truncated_image_keeps_everything_before_the_cut() {
    let image = fixture("xfsdata-default.img");
    let clean = open(&image);
    let fs = Filesystem::mount(clean.clone()).unwrap();
    let sb = fs.superblock().clone();
    let large = fs.lookup_path("/large.bin").unwrap().ino;
    let clean_map = map_of(&clean, &sb, large);
    let (clean_bytes, _) = read_all(&clean, &clean_map);
    // The cut: half way into the file's first piece, off a sector boundary
    // so the sector it falls in is lost whole.
    let first = clean_map.extents[0];
    let cut = first.location.offset + first.location.length / 2 + 100;

    let copy = scratch::Volume::empty(SUITE, &format!("{}-cut.img", std::process::id()), 0);
    {
        let mut source = std::fs::File::open(&image).unwrap();
        let mut head = vec![0u8; cut as usize];
        source.read_exact(&mut head).unwrap();
        std::fs::write(copy.path(), &head).unwrap();
    }
    let device = open(copy.path());

    // Superblocks and group headers past the cut are unreadable; the
    // geometry is the primary's.
    let report = salvage::damage(device.as_ref());
    assert_eq!(report.superblocks.source, Some(0));
    for found in &report.superblocks.copies {
        let past = found.location.offset >= cut;
        assert_eq!(
            found.superblock.is_none(),
            past,
            "superblock {:?}",
            found.ag
        );
        if past {
            assert_eq!(found.damage[0].kind, DamageKind::Unreadable);
        }
    }
    for group in &report.groups {
        let past = group.agf.location.offset >= cut;
        assert_eq!(group.agf.value.is_none(), past, "AGF {}", group.ag);
    }
    assert!(
        report.all().all(|d| d.kind == DamageKind::Unreadable),
        "only what was cut off"
    );

    // The cut file: byte for byte up to the sector the cut falls in, and
    // that sector onwards listed as unreadable.
    let map = map_of(&device, &sb, large);
    let (bytes, unreadable) = read_all(&device, &map);
    let lost_from = first.file_offset + (cut - cut % 512 - first.location.offset);
    assert_eq!(
        &bytes[..lost_from as usize],
        &clean_bytes[..lost_from as usize]
    );
    assert_eq!(
        unreadable.first().map(|e| e.offset),
        Some(lost_from),
        "{unreadable:?}"
    );
    let lost: u64 = unreadable.iter().map(|e| e.length).sum();
    let past_cut: u64 = clean_map
        .extents
        .iter()
        .map(|e| {
            let end = e.location.offset + e.location.length;
            end.saturating_sub((cut - cut % 512).max(e.location.offset))
        })
        .sum();
    assert_eq!(lost, past_cut, "exactly the bytes past the cut are lost");

    // Every other file wholly before the cut reads back to what Linux hashed.
    let manifest = std::fs::read_to_string(image.with_extension("manifest")).unwrap();
    let mut whole = 0;
    let mut lost_files = 0;
    for line in manifest.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields[1] != "file" {
            continue;
        }
        let ino = fs.lookup_path(fields[0]).unwrap().ino;
        let Some(candidate) = salvage::read_inode(device.as_ref(), &sb, ino) else {
            continue;
        };
        let map = salvage::walk_extents(device.as_ref(), &sb, &candidate);
        // A file is whole when its map was read in full and every piece of
        // data it names is before the cut. A map block past the cut loses
        // the pieces it held, which the walk's end says.
        let before = map.end == WalkEnd::Complete
            && map
                .extents
                .iter()
                .all(|e| e.location.offset + e.location.length <= cut - cut % 512);
        let (bytes, unreadable) = read_all(&device, &map);
        if before {
            assert!(unreadable.is_empty(), "{}", fields[0]);
            let digest: String = Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            assert_eq!(digest, fields[3], "{}", fields[0]);
            whole += 1;
        } else {
            assert!(
                !unreadable.is_empty() || matches!(map.end, WalkEnd::Unreadable(_)),
                "{} reaches past the cut and nothing says so",
                fields[0]
            );
            lost_files += 1;
        }
    }
    assert!(whole > 0, "no file lay wholly before the cut");
    assert!(lost_files > 0, "no file reached past the cut");
}
