//! rust-fs-core's conformance helpers, run against this driver's layers on
//! images `mkfs.xfs` and the kernel made, and on damaged copies of them.
//!
//! The helpers check the shape of the shared contract: bounds, flags,
//! streamed counts, read-only by type, the layers agreeing about the same
//! bytes. They cannot tell whether XFS is read correctly, so the File layer
//! is also held to each data fixture's manifest: every file Linux listed
//! is listed, with the size Linux gave and the SHA-256 Linux computed.

mod common;

use common::{fixture, fixtures_matching};
use fs_core::layers::conformance::salvage::{self as salvage_set, Damaged, Driver, Evidence};
use fs_core::layers::conformance::{self, Expected, Limits};
use fs_core::layers::{
    DamageKind, Extent, FileAPI, HealthState, NodeKind, OpenVolume, PrimitiveAPI, VolumeAPI,
};
use fs_core::{BlockRead, FileDevice};
use fs_xfs::layers::{XfsSalvage, XfsVolume};
use fs_xfs::{salvage, Filesystem};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::Arc;

fn source(path: &Path) -> Arc<dyn BlockRead> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

fn limits() -> Limits {
    Limits {
        max_walks: 4096,
        ..Limits::default()
    }
}

struct Entry {
    path: String,
    kind: String,
    size: u64,
    digest: String,
}

fn manifest(image: &Path) -> Vec<Entry> {
    std::fs::read_to_string(image.with_extension("manifest"))
        .unwrap()
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

/// Every geometry and data fixture passes the healthy-volume checks, and
/// each data fixture's directories are where Linux listed them.
#[test]
fn every_fixture_passes_the_healthy_checks() {
    let mut images = fixtures_matching("xfs-", ".img");
    images.extend(fixtures_matching("xfsdata-", ".img"));
    assert!(
        !images.is_empty(),
        "no fixtures: `chore fixtures` builds them"
    );
    for image in images {
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        let volume = XfsVolume::open(source(&image)).unwrap();
        let mut expected = vec![Expected::absent("/no-such-name-in-any-fixture")];
        if image.with_extension("manifest").is_file() {
            expected.extend(
                manifest(&image)
                    .iter()
                    .filter(|e| e.kind == "dir")
                    .map(|e| Expected::directory(&e.path)),
            );
        }
        let report = conformance::check_healthy(&volume, &expected, &limits());
        assert!(report.passed(), "{name}:\n{report}");
    }
}

/// Through the File layer alone, every file the kernel listed is there with
/// the size and the bytes Linux gave it.
#[test]
fn the_file_layer_reads_every_file_linux_listed() {
    let images: Vec<_> = fixtures_matching("xfsdata-", ".img")
        .into_iter()
        .filter(|p| p.with_extension("manifest").is_file())
        .collect();
    assert!(
        !images.is_empty(),
        "no data fixtures: `chore fixtures` builds them"
    );
    for image in images {
        let name = image.file_name().unwrap().to_string_lossy().into_owned();
        let volume = XfsVolume::open(source(&image)).unwrap();
        for entry in manifest(&image) {
            let node = volume.lookup(&entry.path).unwrap();
            let metadata = volume.metadata(&node).unwrap();
            match entry.kind.as_str() {
                "file" => {
                    assert_eq!(metadata.kind, NodeKind::File, "{name}{}", entry.path);
                    assert_eq!(metadata.size, entry.size, "{name}{}", entry.path);
                    let mut hash = Sha256::new();
                    assert_eq!(volume.copy_to(&node, &mut hash).unwrap(), entry.size);
                    let digest = format!("{:x}", hash.finalize());
                    assert_eq!(digest, entry.digest, "{name}{}", entry.path);
                }
                "link" => {
                    assert_eq!(metadata.kind, NodeKind::Symlink, "{name}{}", entry.path);
                    let target = volume.read_link(&node).unwrap();
                    assert_eq!(target, entry.digest.as_bytes(), "{name}{}", entry.path);
                }
                "dir" => assert_eq!(metadata.kind, NodeKind::Directory, "{name}{}", entry.path),
                other => panic!("{name}{}: unknown kind {other}", entry.path),
            }
        }
    }
}

/// An image whose bytes are in memory.
struct Memory(Vec<u8>);

impl BlockRead for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let out_of_bounds = fs_core::Error::OutOfBounds {
            offset,
            len: buf.len() as u64,
            size: self.0.len() as u64,
        };
        let start = usize::try_from(offset).ok();
        let bytes = start
            .and_then(|start| self.0.get(start..start.checked_add(buf.len())?))
            .ok_or(out_of_bounds)?;
        buf.copy_from_slice(bytes);
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

/// Reads every byte through one scratch buffer. The shared salvage set needs
/// a `Vec<u8>` for each case, but leaving its all-zero blocks untouched lets
/// the OS back most of each sparse fixture with shared zero pages.
fn snapshot(source: &dyn BlockRead) -> fs_core::Result<Vec<u8>> {
    const READ_SIZE: usize = 1 << 20;
    const BLOCK_SIZE: usize = 4096;
    let mut bytes = vec![0u8; source.size_bytes() as usize];
    let mut scratch = vec![0u8; READ_SIZE];
    for (index, chunk) in bytes.chunks_mut(READ_SIZE).enumerate() {
        let input = &mut scratch[..chunk.len()];
        source.read_at(index as u64 * READ_SIZE as u64, input)?;
        for (output, block) in chunk.chunks_mut(BLOCK_SIZE).zip(input.chunks(BLOCK_SIZE)) {
            if block.iter().any(|&byte| byte != 0) {
                output.copy_from_slice(block);
            }
        }
    }
    Ok(bytes)
}

fn invert(image: &mut [u8], at: u64) {
    image[at as usize] = !image[at as usize];
}

/// The salvage set's view of this driver: the default data fixture, healthy
/// and with five kinds of damage, each made at bytes the format fixes.
struct Xfs {
    healthy: Arc<dyn BlockRead>,
}

impl Xfs {
    fn new() -> Self {
        let device = source(&fixture("xfsdata-default.img"));
        Self {
            healthy: Arc::new(Memory(snapshot(device.as_ref()).unwrap())),
        }
    }
}

impl Driver for Xfs {
    type Adapter = XfsSalvage;
    type StrictError = String;

    fn salvage(&self, source: &dyn BlockRead) -> XfsSalvage {
        // Built from whatever geometry the copies give; a source with none
        // gets the healthy image's, so its verbs still run and report.
        let sb = salvage::superblocks(source)
            .geometry
            .or_else(|| salvage::superblocks(self.healthy.as_ref()).geometry)
            .expect("the healthy image has a geometry");
        XfsSalvage::new(sb)
    }

    /// The strict API: mount, read every directory and file, and check the
    /// volume. Any refusal or finding rejects it.
    fn strict(&self, source: &dyn BlockRead) -> Result<(), String> {
        let bytes = snapshot(source).map_err(|e| format!("reading the source: {e}"))?;
        let fs = Filesystem::mount(Arc::new(Memory(bytes))).map_err(|e| e.to_string())?;
        let mut stack = vec![fs.root_inode().map_err(|e| e.to_string())?];
        while let Some(dir) = stack.pop() {
            let (inode, raw) = fs.read_inode_raw(dir.ino).map_err(|e| e.to_string())?;
            for entry in fs.read_dir(&inode, &raw).map_err(|e| e.to_string())? {
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                let (child, raw) = fs.read_inode_raw(entry.ino).map_err(|e| e.to_string())?;
                if child.is_dir() {
                    stack.push(child);
                } else if child.is_regular_file() {
                    fs.read_file(&child, &raw).map_err(|e| e.to_string())?;
                }
            }
        }
        let report = fs_xfs::check::check(&fs);
        if report.is_clean() {
            Ok(())
        } else {
            Err(format!("{report:?}"))
        }
    }

    fn healthy(&self) -> Vec<u8> {
        snapshot(self.healthy.as_ref()).unwrap()
    }

    fn damaged(&self) -> Vec<Damaged> {
        let device = self.healthy.as_ref();
        let sb = salvage::superblocks(device).geometry.unwrap();
        let fs = Filesystem::mount(Arc::clone(&self.healthy)).unwrap();
        let fragmented = fs.lookup_path("/fragmented.bin").unwrap().ino;
        let small = fs.lookup_path("/small.txt").unwrap().ino;
        let sector = u64::from(sb.sectsize);
        let leaf = {
            let candidate = salvage::read_inode(device, &sb, fragmented).unwrap();
            *salvage::walk_extents(device, &sb, &candidate)
                .map_blocks
                .last()
                .unwrap()
        };

        let mut primary = self.healthy();
        invert(&mut primary, 400);
        let mut agf = self.healthy();
        invert(&mut agf, sb.ag_offset(1) + sector);
        let mut inode = self.healthy();
        invert(&mut inode, salvage::inode_location(&sb, small).offset + 39);
        let mut map = self.healthy();
        invert(&mut map, leaf.offset);
        vec![
            Damaged::new(
                "primary superblock checksum",
                primary,
                Evidence::Damage(DamageKind::Checksum),
            ),
            Damaged::new("AGF 1 magic", agf, Evidence::Damage(DamageKind::Signature)),
            Damaged::new("inode checksum", inode, Evidence::FailedCandidate),
            Damaged::new("block map leaf magic", map, Evidence::BrokenWalk),
            Damaged::new(
                "AGI 2 unreadable",
                self.healthy(),
                Evidence::Damage(DamageKind::Unreadable),
            )
            .unreadable(Extent::new(sb.ag_offset(2) + 2 * sector, sector)),
        ]
    }
}

/// The salvage verbs keep every rule of the shared salvage set.
#[test]
fn the_salvage_verbs_conform() {
    salvage_set::run(&Xfs::new()).assert_conforms();
}

/// Damaged copies opened for salvage pass the damaged-volume checks and
/// still give the salvage verbs a geometry; where the damage is in the
/// volume's own structures, the volume does not call itself clean.
#[test]
fn damaged_copies_pass_the_damaged_checks() {
    let driver = Xfs::new();
    for case in driver.damaged() {
        let source: Arc<dyn BlockRead> = Arc::new(Memory(case.image));
        let volume = XfsVolume::open_for_salvage(source).unwrap();
        let report = conformance::check_damaged(&volume, &limits());
        assert!(report.passed(), "{}:\n{report}", case.name);
        assert!(volume.adapter().is_ok(), "{}", case.name);
        // Health covers the volume's own structures (superblocks, group
        // headers, the log), not every file's map: damage to one inode or
        // one block map is the salvage verbs' to find.
        if case.name.contains("superblock") || case.name.contains("AGF") {
            assert_ne!(
                volume.health().unwrap().state,
                HealthState::Clean,
                "{}",
                case.name
            );
        }
    }
}
