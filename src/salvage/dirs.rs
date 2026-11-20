//! Directory entries read for salvage, and the tree rebuilt from them.

use super::evidence::WalkEnd;
use super::evidence::{finding, Damage, DamageKind, Extent, Severity};
use super::inodes::{self, ExtentMap, InodeCandidate, InodeState};
use crate::dir::{self, DirEntry};
use crate::inode::{FileType, Format};
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::collections::BTreeMap;

/// One name found in a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamedEntry {
    /// The name exactly as stored: XFS names are bytes, not text.
    pub name: Vec<u8>,
    /// The inode the name points at.
    pub ino: u64,
    /// The file type the entry records, when the volume records one.
    pub ftype: Option<FileType>,
}

/// What could be read of one directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirectoryListing {
    /// The directory's inode number.
    pub ino: u64,
    /// The parent its `..` names, when that could be read.
    pub parent: Option<u64>,
    /// Every other name, `.` and `..` left out, in on-disk order.
    pub entries: Vec<NamedEntry>,
    /// How the walk of the directory's data blocks ended.
    pub end: WalkEnd,
    pub damage: Vec<Damage>,
}

/// Read the entries of directory `candidate`: a short-form directory out
/// of the inode, or every data block its map reaches.
///
/// A record that does not check out is reported and skipped, and the read
/// goes on. A v5 block that names another directory as its owner is
/// reported and not read: its names are someone else's.
pub fn directory_entries(
    device: &dyn BlockRead,
    sb: &Superblock,
    candidate: &InodeCandidate,
) -> DirectoryListing {
    let structure = format!("directory {}", candidate.ino);
    let mut listing = DirectoryListing {
        ino: candidate.ino,
        parent: None,
        entries: Vec::new(),
        end: WalkEnd::Complete,
        damage: Vec::new(),
    };
    let Some(inode) = &candidate.inode else {
        listing.end = WalkEnd::Broken;
        return listing;
    };
    if inode.format == Format::Local {
        let mut buf = vec![0u8; usize::from(sb.inodesize)];
        if device.read_at(candidate.location.offset, &mut buf).is_err() {
            listing.end = WalkEnd::Unreadable(candidate.location);
            return listing;
        }
        let (start, end) = inode.data_fork_range(buf.len());
        let (parent, entries, stopped) =
            dir::salvage_short_form(&buf[start.min(end)..end], inode.size, sb);
        listing.parent = parent;
        listing.entries = entries.into_iter().map(named).collect();
        if let Some(why) = stopped {
            listing.end = WalkEnd::Broken;
            listing.damage.push(finding(
                DamageKind::Malformed,
                Severity::Error,
                candidate.location,
                structure,
                format!("short-form directory: {why}"),
            ));
        }
        return listing;
    }
    let map = inodes::walk_extents(device, sb, candidate);
    listing.end = map.end;
    listing.damage.extend(map.damage.iter().cloned());
    let block = sb.dirblocksize() as usize;
    let mut buf = vec![0u8; block];
    let mut at = 0u64;
    while at < map.claimed {
        let Some(location) = source_of(&map, at) else {
            // A hole in the data space: no block was ever written here.
            at += block as u64;
            continue;
        };
        let (_, unreadable) = inodes::read_mapped(device, &map, at, &mut buf);
        if !unreadable.is_empty() {
            listing.end = WalkEnd::Unreadable(Extent::new(location, block as u64));
            listing.damage.push(finding(
                DamageKind::Unreadable,
                Severity::Error,
                Extent::new(location, block as u64),
                structure.clone(),
                format!("directory block at offset {at}"),
            ));
            at += block as u64;
            continue;
        }
        read_block(sb, &buf, candidate.ino, location, &structure, &mut listing);
        at += block as u64;
    }
    listing
}

/// The source byte of file offset `at`, if the map places it.
fn source_of(map: &ExtentMap, at: u64) -> Option<u64> {
    map.extents
        .iter()
        .find(|e| e.file_offset <= at && at < e.file_offset + e.location.length)
        .map(|e| e.location.offset + (at - e.file_offset))
}

fn read_block(
    sb: &Superblock,
    buf: &[u8],
    owner: u64,
    location: u64,
    structure: &str,
    listing: &mut DirectoryListing,
) {
    let place = Extent::new(location, buf.len() as u64);
    let mut finding = |kind, detail: String| {
        listing
            .damage
            .push(finding(kind, Severity::Error, place, structure, detail));
    };
    let Some(block) = dir::salvage_data_block(buf, sb) else {
        finding(
            DamageKind::Signature,
            format!(
                "the block at byte {location} is not a directory data block (magic {:#010x})",
                u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]])
            ),
        );
        listing.end = WalkEnd::Broken;
        return;
    };
    if let Some(stated) = block.owner {
        if stated != owner {
            finding(
                DamageKind::Mismatch,
                format!("the block says it belongs to directory {stated}"),
            );
            listing.end = WalkEnd::Broken;
            return;
        }
    }
    if block.checksum == Some(false) {
        finding(DamageKind::Checksum, "the block fails its checksum".into());
    }
    for (offset, why) in &block.skipped {
        finding(
            DamageKind::Malformed,
            format!("at byte {offset} of the block: {why}"),
        );
    }
    for entry in block.entries {
        match entry.name.as_slice() {
            b"." => {}
            b".." => listing.parent = Some(entry.ino),
            _ => listing.entries.push(named(entry)),
        }
    }
}

fn named(entry: DirEntry) -> NamedEntry {
    NamedEntry {
        name: entry.name,
        ino: entry.ino,
        ftype: entry.ftype,
    }
}

/// A claim that an inode sits under another one, or at the root.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Parent {
    /// The inode is the volume's root directory.
    Root,
    /// A directory holds a name for it.
    Directory { ino: u64, name: Vec<u8> },
}

/// One inode's place in the rebuilt tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    pub ino: u64,
    /// Every claim found. None makes an orphan; a file with several hard
    /// links has one per name, and a directory with more than one is a
    /// conflict. Claims are evidence, and may form cycles.
    pub parents: Vec<Parent>,
}

/// Parent claims among a set of candidates, one node per candidate in
/// input order, and every directory's listing they came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree {
    pub nodes: Vec<TreeNode>,
    pub directories: Vec<DirectoryListing>,
}

impl Tree {
    /// Nodes that no directory names and that are not the root.
    pub fn orphans(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.iter().filter(|node| node.parents.is_empty())
    }

    /// Nodes with more than one claim.
    pub fn conflicts(&self) -> impl Iterator<Item = &TreeNode> {
        self.nodes.iter().filter(|node| node.parents.len() > 1)
    }
}

/// Rebuild parent claims among `candidates` from the entries of every live
/// directory among them. A name pointing at an inode not among the
/// candidates is kept in its directory's listing and makes no node.
pub fn rebuild_tree(
    device: &dyn BlockRead,
    sb: &Superblock,
    candidates: &[InodeCandidate],
) -> Tree {
    let mut claims: BTreeMap<u64, Vec<Parent>> = BTreeMap::new();
    let mut directories = Vec::new();
    for candidate in candidates {
        let is_dir = candidate.state == InodeState::Live
            && candidate.inode.as_ref().is_some_and(|i| i.is_dir());
        if !is_dir {
            continue;
        }
        let listing = directory_entries(device, sb, candidate);
        for entry in &listing.entries {
            claims
                .entry(entry.ino)
                .or_default()
                .push(Parent::Directory {
                    ino: candidate.ino,
                    name: entry.name.clone(),
                });
        }
        directories.push(listing);
    }
    let nodes = candidates
        .iter()
        .map(|candidate| {
            let mut parents = claims.get(&candidate.ino).cloned().unwrap_or_default();
            if candidate.ino == sb.rootino {
                parents.insert(0, Parent::Root);
            }
            TreeNode {
                ino: candidate.ino,
                parents,
            }
        })
        .collect();
    Tree { nodes, directories }
}
