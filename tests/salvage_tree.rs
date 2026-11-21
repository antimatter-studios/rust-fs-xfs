//! The directory tree rebuilt from raw inodes, on the data fixtures the
//! kernel wrote, and on damaged copies of them.
//!
//! The oracle is each fixture's `.manifest`, the list of every path Linux
//! found in the volume it had just written. The tree is rebuilt from an
//! inode scan of the whole device and nothing else, no mount, and every
//! path it reaches from the root has to be one Linux listed, and every
//! path Linux listed has to be reached.

mod common;

use common::{fixture, fixtures_matching, scratch};
use fs_core::{BlockRead, FileDevice};
use fs_xfs::salvage::{self, DamageKind, Extent, InodeCandidate, Parent, Tree, WalkEnd};
use fs_xfs::{Filesystem, Superblock};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

const SUITE: &str = "salvage_tree";

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

fn manifest_paths(image: &Path) -> BTreeSet<String> {
    let path = image.with_extension("manifest");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
        .lines()
        .filter_map(|l| l.split('\t').next().map(str::to_string))
        .collect()
}

/// Every live inode on the device, by scan.
fn scan(device: &FileDevice, image: &Path) -> (Superblock, Vec<InodeCandidate>) {
    let sb = salvage::superblocks(device).geometry.expect("a geometry");
    let length = std::fs::metadata(image).unwrap().len();
    let mut found = Vec::new();
    salvage::find_inodes(device, &sb, Extent::new(0, length), &mut |c| {
        if c.state == salvage::InodeState::Live {
            found.push(c);
        }
        true
    });
    (sb, found)
}

/// Every path the tree reaches from the root, each inode's names expanded
/// through its parents' paths.
fn paths(tree: &Tree) -> BTreeSet<String> {
    let mut children: BTreeMap<u64, Vec<(u64, String)>> = BTreeMap::new();
    let mut root = None;
    for node in &tree.nodes {
        for parent in &node.parents {
            match parent {
                Parent::Root => root = Some(node.ino),
                Parent::Directory { ino, name } => children
                    .entry(*ino)
                    .or_default()
                    .push((node.ino, String::from_utf8_lossy(name).into_owned())),
            }
        }
    }
    let mut out = BTreeSet::new();
    let mut stack = vec![(root.expect("the root is found"), String::new())];
    let mut depth_guard = 0;
    while let Some((ino, path)) = stack.pop() {
        depth_guard += 1;
        assert!(depth_guard < 100_000, "the tree has a cycle");
        for (child, name) in children.get(&ino).into_iter().flatten() {
            let child_path = format!("{path}/{name}");
            out.insert(child_path.clone());
            stack.push((*child, child_path));
        }
    }
    out
}

/// The two sets of paths are the same, or the differences are named.
fn same_paths(ours: &BTreeSet<String>, linux: &BTreeSet<String>, what: &str) {
    let extra: Vec<_> = ours.difference(linux).collect();
    let missing: Vec<_> = linux.difference(ours).collect();
    assert!(
        extra.is_empty() && missing.is_empty(),
        "{what}: reached but not listed by Linux {extra:?}; listed but not reached {missing:?}"
    );
}

/// On every data fixture, the tree rebuilt from a scan reaches exactly the
/// paths Linux listed, and every directory reads without damage.
#[test]
fn the_rebuilt_tree_reaches_every_path_linux_listed() {
    let images: Vec<_> = fixtures_matching("xfsdata-", ".img")
        .into_iter()
        .filter(|p| p.with_extension("manifest").is_file())
        .collect();
    assert!(
        !images.is_empty(),
        "no xfsdata fixture: `chore fixtures` builds them"
    );
    for image in images {
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        let device = open(&image);
        let (sb, candidates) = scan(&device, &image);
        let tree = salvage::rebuild_tree(device.as_ref(), &sb, &candidates);
        for listing in &tree.directories {
            assert_eq!(listing.end, WalkEnd::Complete, "{name}: {listing:#?}");
            assert!(listing.damage.is_empty(), "{name}: {:#?}", listing.damage);
        }
        same_paths(&paths(&tree), &manifest_paths(&image), &name);
        let directories: Vec<_> = tree
            .nodes
            .iter()
            .filter(|n| tree.directories.iter().any(|d| d.ino == n.ino))
            .collect();
        for directory in directories {
            assert!(
                directory.parents.len() <= 2,
                "{name}: directory {} has {:?}",
                directory.ino,
                directory.parents
            );
        }
    }
}

fn damaged_copy(tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &fixture("xfsdata-default.img"),
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

/// With a directory's inode destroyed, the files it held are not lost:
/// they are orphans the scan still found, and everything else is in place.
#[test]
fn files_under_a_destroyed_directory_become_orphans() {
    let image = fixture("xfsdata-default.img");
    let (dir_ino, file_ino) = {
        let fs = Filesystem::mount(open(&image)).unwrap();
        (
            fs.lookup_path("/sub/nested").unwrap().ino,
            fs.lookup_path("/sub/nested/file.txt").unwrap().ino,
        )
    };
    let copy = damaged_copy("dir-inode");
    let sb = salvage::superblocks(open(copy.path()).as_ref())
        .geometry
        .unwrap();
    invert(copy.path(), salvage::inode_location(&sb, dir_ino).offset);
    let device = open(copy.path());
    let (sb, candidates) = scan(&device, copy.path());
    assert!(candidates.iter().all(|c| c.ino != dir_ino));
    let tree = salvage::rebuild_tree(device.as_ref(), &sb, &candidates);
    let orphans: Vec<u64> = tree.orphans().map(|n| n.ino).collect();
    assert!(orphans.contains(&file_ino), "{orphans:?}");
    // The directory's own name is still in /sub's listing, but with no
    // inode behind it there is no node to put there.
    let mut want = manifest_paths(&image);
    want.retain(|p| p != "/sub/nested" && !p.starts_with("/sub/nested/"));
    same_paths(&paths(&tree), &want, "destroyed /sub/nested");
    let sub = tree
        .directories
        .iter()
        .find(|d| d.entries.iter().any(|e| e.ino == dir_ino))
        .expect("/sub still names the destroyed directory");
    assert!(sub.damage.is_empty());
}

/// An entry whose record is damaged is reported and skipped, and the other
/// names in its block are kept.
#[test]
fn a_damaged_entry_is_skipped_and_its_neighbours_kept() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let (sb, candidates) = scan(&device, &image);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let manyfiles = fs.lookup_path("/manyfiles").unwrap().ino;
    let victim = fs.lookup_path("/manyfiles/entry-1.txt").unwrap().ino;
    let dir = candidates.iter().find(|c| c.ino == manyfiles).unwrap();
    let clean = salvage::directory_entries(device.as_ref(), &sb, dir);
    assert!(clean.damage.is_empty());
    // Find the entry's record on the source: its 8-byte inode number,
    // followed by its name length and name.
    let map = salvage::walk_extents(device.as_ref(), &sb, dir);
    let mut record = None;
    'search: for piece in &map.extents {
        let mut bytes = vec![0u8; piece.location.length as usize];
        device.read_at(piece.location.offset, &mut bytes).unwrap();
        let mut needle = victim.to_be_bytes().to_vec();
        needle.push(b"entry-1.txt".len() as u8);
        needle.extend_from_slice(b"entry-1.txt");
        for (at, window) in bytes.windows(needle.len()).enumerate() {
            if window == needle.as_slice() {
                record = Some(piece.location.offset + at as u64);
                break 'search;
            }
        }
    }
    let record = record.expect("the entry's record is in the directory's blocks");

    let copy = damaged_copy("dir-entry");
    // The name length: the record no longer ends where its tag says.
    invert(copy.path(), record + 8);
    let device = open(copy.path());
    let listing = salvage::directory_entries(device.as_ref(), &sb, dir);
    let names: BTreeSet<Vec<u8>> = listing.entries.iter().map(|e| e.name.clone()).collect();
    assert!(!names.contains(b"entry-1.txt".as_slice()));
    assert_eq!(
        names.len(),
        clean.entries.len() - 1,
        "only the damaged entry is lost"
    );
    assert!(
        listing
            .damage
            .iter()
            .any(|d| matches!(d.kind, DamageKind::Malformed | DamageKind::Checksum)),
        "{:#?}",
        listing.damage
    );
}
