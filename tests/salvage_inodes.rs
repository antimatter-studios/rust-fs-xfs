//! Inodes found and files read back through the salvage reads, on the
//! data fixtures the kernel wrote, and on damaged copies of them.
//!
//! The oracle is each fixture's `.manifest`: every path, its type, its
//! size and the SHA-256 of its contents (or a symbolic link's target),
//! computed in the guest by Linux from the files the kernel wrote. A file
//! read through [`salvage::walk_extents`] and [`salvage::read_mapped`] has
//! to hash to what Linux hashed. The mounted driver is used only to say
//! which inode a path is; nothing it reads is compared against.

mod common;

use common::{fixture, fixtures_matching, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, DamageKind, Extent, InodeState, Integrity, WalkEnd};
use fs_xfs::{Filesystem, Superblock};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

const SUITE: &str = "salvage_inodes";

struct Entry {
    path: String,
    kind: String,
    size: u64,
    digest: String,
}

fn manifest(image: &Path) -> Vec<Entry> {
    let path = image.with_extension("manifest");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some(Entry {
                path: parts.next()?.to_string(),
                kind: parts.next()?.to_string(),
                size: parts.next()?.parse().ok()?,
                digest: parts.next()?.to_string(),
            })
        })
        .collect()
}

fn data_fixtures() -> Vec<std::path::PathBuf> {
    let out: Vec<_> = fixtures_matching("xfsdata-", ".img")
        .into_iter()
        .filter(|p| p.with_extension("manifest").is_file())
        .collect();
    assert!(
        !out.is_empty(),
        "no xfsdata-*.img fixture with a .manifest: `chore fixtures` builds them"
    );
    out
}

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

fn geometry(device: &FileDevice) -> Superblock {
    salvage::superblocks(device)
        .geometry
        .expect("the fixture has a geometry")
}

/// The whole of a file, read through its salvage map.
fn salvaged_bytes(device: &FileDevice, sb: &Superblock, ino: u64) -> (Vec<u8>, salvage::ExtentMap) {
    let candidate = salvage::read_inode(device, sb, ino).expect("an inode is there");
    let map = salvage::walk_extents(device, sb, &candidate);
    let size = map.inline.map_or(map.claimed, |inline| inline.length);
    let mut bytes = vec![0u8; size as usize];
    let (read, unreadable) = salvage::read_mapped(device, &map, 0, &mut bytes);
    assert_eq!(read as u64, size);
    assert!(unreadable.is_empty(), "{unreadable:?}");
    (bytes, map)
}

/// A remote symbolic link's target, read through its salvage map.
///
/// On a v5 volume every block of the target starts with a 56-byte `XSLM`
/// header, so the file bytes `read_mapped` returns are header, then the
/// target, cut short by the header's own length. The target is what the
/// manifest hashed, so each block gives up its header and holds only the
/// bytes its header says it does.
fn remote_symlink_target(
    device: &FileDevice,
    sb: &Superblock,
    map: &salvage::ExtentMap,
) -> Vec<u8> {
    use fs_core::BlockRead;
    use fs_xfs::format::symlink::{offsets, XFS_SYMLINK_HDR_SIZE, XFS_SYMLINK_MAGIC};
    let block = u64::from(sb.blocksize);
    let want = map.claimed as usize;
    let mut target = Vec::new();
    for extent in &map.extents {
        let mut at = extent.location.offset;
        let end = extent.location.offset + extent.location.length.max(block);
        while target.len() < want && at < end {
            let mut head = vec![0u8; XFS_SYMLINK_HDR_SIZE];
            device.read_at(at, &mut head).expect("a symlink block");
            let v5 =
                u32::from_be_bytes(head[offsets::MAGIC..offsets::MAGIC + 4].try_into().unwrap())
                    == XFS_SYMLINK_MAGIC;
            let (skip, holds) = if v5 {
                let bytes = u32::from_be_bytes(
                    head[offsets::BYTES..offsets::BYTES + 4].try_into().unwrap(),
                );
                (XFS_SYMLINK_HDR_SIZE, bytes as usize)
            } else {
                (0, block as usize)
            };
            let take = holds.min(want - target.len());
            let mut body = vec![0u8; take];
            device
                .read_at(at + skip as u64, &mut body)
                .expect("a symlink block");
            target.extend_from_slice(&body);
            at += block;
        }
    }
    target
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Every file and link of every data fixture reads back, through the
/// salvage map alone, to the bytes Linux hashed; every inode passes its
/// checks, and every walk is complete.
#[test]
fn every_file_reads_back_to_what_linux_hashed() {
    for image in data_fixtures() {
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        let device = open(&image);
        let fs = Filesystem::mount(device.clone()).unwrap();
        let sb = geometry(&device);
        let mut checked = 0;
        for entry in manifest(&image) {
            if entry.kind == "dir" {
                continue;
            }
            let ino = fs.lookup_path(&entry.path).unwrap().ino;
            let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
            assert_eq!(candidate.state, InodeState::Live, "{name}{}", entry.path);
            assert_ne!(
                candidate.integrity,
                Integrity::Failed,
                "{name}{}",
                entry.path
            );
            assert!(
                candidate.damage.is_empty(),
                "{name}{}: {:#?}",
                entry.path,
                candidate.damage
            );
            let (bytes, map) = salvaged_bytes(&device, &sb, ino);
            assert_eq!(
                map.end,
                WalkEnd::Complete,
                "{name}{}: {:#?}",
                entry.path,
                map.damage
            );
            assert!(
                map.damage.is_empty(),
                "{name}{}: {:#?}",
                entry.path,
                map.damage
            );
            match entry.kind.as_str() {
                "file" => {
                    assert_eq!(bytes.len() as u64, entry.size, "{name}{}", entry.path);
                    assert_eq!(sha256(&bytes), entry.digest, "{name}{}", entry.path);
                }
                "link" => {
                    let target = if map.inline.is_some() {
                        bytes
                    } else {
                        remote_symlink_target(&device, &sb, &map)
                    };
                    assert_eq!(
                        String::from_utf8_lossy(&target),
                        entry.digest,
                        "{name}{}",
                        entry.path
                    )
                }
                other => panic!(
                    "{name}{}: a manifest kind {other} this test does not know",
                    entry.path
                ),
            }
            checked += 1;
        }
        assert!(checked > 100, "{name}: only {checked} files checked");
    }
}

/// A scan of the whole device finds every inode the manifest's paths name,
/// live and intact, at the number its position gives it.
#[test]
fn a_scan_of_the_device_finds_every_inode() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let sb = geometry(&device);
    let mut found = HashSet::new();
    let length = std::fs::metadata(&image).unwrap().len();
    let scan = salvage::find_inodes(device.as_ref(), &sb, Extent::new(0, length), &mut |c| {
        if c.state == InodeState::Live && c.damage.is_empty() {
            found.insert(c.ino);
        }
        true
    });
    assert!(scan.unreadable.is_empty());
    assert_eq!(scan.stopped_at, None);
    for entry in manifest(&image) {
        let ino = fs.lookup_path(&entry.path).unwrap().ino;
        assert!(
            found.contains(&ino),
            "{} (inode {ino}) was not found",
            entry.path
        );
    }
    assert!(found.contains(&sb.rootino));
}

/// A scan stopped by its callback says where, and resuming there finds the
/// rest without repeating any.
#[test]
fn a_stopped_scan_resumes_where_it_stopped() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let sb = geometry(&device);
    let length = std::fs::metadata(&image).unwrap().len();
    let mut all = Vec::new();
    salvage::find_inodes(device.as_ref(), &sb, Extent::new(0, length), &mut |c| {
        all.push(c.ino);
        true
    });
    let mut first = Vec::new();
    let stopped = salvage::find_inodes(device.as_ref(), &sb, Extent::new(0, length), &mut |c| {
        first.push(c.ino);
        first.len() < 10
    });
    let at = stopped.stopped_at.expect("stopped");
    let mut rest = Vec::new();
    salvage::find_inodes(
        device.as_ref(),
        &sb,
        Extent::new(at, length - at),
        &mut |c| {
            rest.push(c.ino);
            true
        },
    );
    first.extend(rest);
    assert_eq!(first, all);
}

fn damaged_copy(source: &str, tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &fixture(source),
        &format!("{}-{tag}.img", std::process::id()),
    )
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

/// The inode of the B+tree-mapped file in the default data fixture, and
/// its digest.
fn fragmented() -> (u64, String) {
    let image = fixture("xfsdata-default.img");
    let fs = Filesystem::mount(open(&image)).unwrap();
    let digest = manifest(&image)
        .into_iter()
        .find(|e| e.path == "/fragmented.bin")
        .expect("the manifest lists /fragmented.bin")
        .digest;
    (fs.lookup_path("/fragmented.bin").unwrap().ino, digest)
}

/// An inode whose checksum fails is still found, decoded and walked, and
/// its file still reads back to what Linux hashed.
#[test]
fn an_inode_that_fails_its_checksum_still_yields_its_file() {
    let (ino, digest) = fragmented();
    let copy = damaged_copy("xfsdata-default.img", "inode-crc");
    let sb = geometry(&open(copy.path()));
    // The low byte of di_atime's nanoseconds: covered by the checksum,
    // and no part of the data's map.
    invert(copy.path(), salvage::inode_location(&sb, ino).offset + 39);
    let device = open(copy.path());
    let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
    assert_eq!(candidate.integrity, Integrity::Failed);
    assert_eq!(candidate.damage.len(), 1, "{:#?}", candidate.damage);
    assert_eq!(candidate.damage[0].kind, DamageKind::Checksum);
    let (bytes, map) = salvaged_bytes(&device, &sb, ino);
    assert_eq!(map.end, WalkEnd::Complete);
    assert_eq!(sha256(&bytes), digest);
    // The strict reader refuses the same inode.
    let fs = Filesystem::mount(device.clone()).unwrap();
    assert!(fs.read_inode(ino).is_err());
}

/// A block of the file's map that fails its checksum is reported and still
/// used; one whose magic is gone is reported and its records are lost, and
/// the walk says it broke.
#[test]
fn damaged_map_blocks_are_reported_and_used_where_they_can_be() {
    let (ino, digest) = fragmented();
    let sb = geometry(&open(&fixture("xfsdata-default.img")));
    let clean = {
        let device = open(&fixture("xfsdata-default.img"));
        let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
        salvage::walk_extents(device.as_ref(), &sb, &candidate)
    };
    assert!(
        !clean.map_blocks.is_empty(),
        "the file is mapped by a B+tree"
    );
    let leaf = clean.map_blocks[clean.map_blocks.len() - 1];

    // The last byte of the leaf, past its records: covered by the
    // checksum, read by nothing.
    let copy = damaged_copy("xfsdata-default.img", "bmbt-crc");
    invert(copy.path(), leaf.offset + leaf.length - 1);
    let device = open(copy.path());
    let (bytes, map) = salvaged_bytes(&device, &sb, ino);
    assert_eq!(map.end, WalkEnd::Complete);
    assert_eq!(map.damage.len(), 1, "{:#?}", map.damage);
    assert_eq!(map.damage[0].kind, DamageKind::Checksum);
    assert_eq!(map.damage[0].location, leaf);
    assert_eq!(sha256(&bytes), digest);

    // Its magic: the leaf is not a block-map block any more.
    let copy = damaged_copy("xfsdata-default.img", "bmbt-magic");
    invert(copy.path(), leaf.offset);
    let device = open(copy.path());
    let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
    let map = salvage::walk_extents(device.as_ref(), &sb, &candidate);
    assert_eq!(map.end, WalkEnd::Broken);
    assert!(
        map.damage
            .iter()
            .any(|d| d.kind == DamageKind::Signature && d.location == leaf),
        "{:#?}",
        map.damage
    );
    assert!(map.mapped() < clean.mapped());
    assert!(!map.map_blocks.contains(&leaf));
}
