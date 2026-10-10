//! Offline repair of redundant directory metadata (#394).
//!
//! Names and ordinary entry inode numbers are authoritative: neither is
//! guessed. Types, tags, dot entries and hash indexes are derived from
//! validated allocated inodes, directory topology and the entry data.
//! Every directory is planned before any write, as a repair-planner rule
//! ([`DirectoryMetadata`]): the volume with the proposed changes applied must
//! check complete and free of every directory finding, and
//! [`crate::repair::apply`] reads every buffer back before writing. An
//! ambiguous name, target or topology therefore leaves the entire image
//! unchanged, including otherwise repairable damage elsewhere.
//!
//! Layout reference: <https://www.kernel.org/pub/linux/utils/fs/xfs/docs/xfs_filesystem_structure.pdf>,
//! chapter "Directories"; offsets are shared with the ordinary parsers.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use fs_core::BlockRead;

use crate::check::{self, Code, Report};
use crate::dir::{self, offsets, DirEntry};
use crate::endian::{be16, be32, be64};
use crate::error::{Error, Result};
use crate::format::dir::*;
use crate::fs::Filesystem;
use crate::inode::{Format, Inode};
use crate::inode_btree::{self, Which, INODES_PER_CHUNK};
use crate::repair::{Proposal, Rule};
use crate::superblock::crc32c_with_zeroed_crc;

struct Patch {
    after: Vec<u8>,
}

struct Block {
    bytes: Vec<u8>,
    original: Vec<u8>,
    /// One physical byte offset per filesystem block, including split extents.
    locations: Vec<u64>,
}

struct Directory {
    inode: Inode,
    raw: Vec<u8>,
    original: Vec<u8>,
    entries: Vec<(u32, DirEntry)>,
    blocks: BTreeMap<u32, Block>,
    /// Filesystem blocks in one directory block. `blocks` is keyed by
    /// directory block; the index's child and sibling pointers count
    /// filesystem blocks, as every `xfs_dablk_t` does.
    per: u32,
    /// Parent inode field: inode-relative for short form, block-relative otherwise.
    parent: Option<(Option<u32>, usize, usize)>,
}

impl Directory {
    /// The directory block an index pointer names, which has to be the
    /// first filesystem block of one.
    fn block_of(&self, dablk: u32) -> Result<u32> {
        if !dablk.is_multiple_of(self.per) {
            return Err(refuse(
                "index pointer is not the start of a directory block",
            ));
        }
        Ok(dablk / self.per)
    }
}

fn refuse(what: impl Into<String>) -> Error {
    Error::BadSuperblock(format!("directory repair refused: {}", what.into()))
}

/// Directory findings this rule owns, or accounts for when they stop the
/// check's walk: the preview it builds must be free of every one.
pub(crate) const CODES: &[Code] = &[
    Code::DirEntryFtype,
    Code::DirUnreadable,
    Code::DirNotADirectory,
];

/// Directory entry types, tags, dot entries and hash indexes, derived from
/// validated inodes, topology and the entries' own data (#394).
///
/// Names and ordinary entry inode numbers are never changed. The rule runs
/// whether or not the check found anything, since several of what it
/// repairs (a stale index slot, a redundant tag) are not findings: it
/// proposes nothing when nothing differs.
pub struct DirectoryMetadata;

impl Rule for DirectoryMetadata {
    fn name(&self) -> &'static str {
        "directory-metadata"
    }

    fn repairs(&self) -> &'static [Code] {
        CODES
    }

    fn completes(&self) -> &'static [Code] {
        &[Code::DirUnreadable, Code::DirNotADirectory]
    }

    fn propose(&self, fs: &Filesystem, report: &Report, proposal: &mut Proposal) -> Result<()> {
        // Inode allocation is read before any directory is, and the inode
        // rule owns its faults: a directory is left to the next run once
        // the inodes under it are settled.
        if report
            .findings
            .iter()
            .any(|f| crate::inode_repair::CODES.contains(&f.code))
        {
            return Ok(());
        }
        for (at, patch) in plan_changes(fs)? {
            proposal.put(
                at,
                patch.after,
                Code::DirEntryFtype,
                "rebuild a directory's types, tags, dot entries and index from its entries",
            )?;
        }
        Ok(())
    }
}

/// Every change the repair makes, checked on a preview of the volume.
fn plan_changes(fs: &Filesystem) -> Result<BTreeMap<u64, Patch>> {
    if !fs.sb.is_v5() || !fs.sb.has_ftype() || fs.was_replayed() {
        return Err(refuse(
            "only a clean v5 filesystem with file types is supported",
        ));
    }
    let allocated = allocated_inodes(fs)?;
    let mut directories = BTreeMap::new();
    for (&ino, (inode, raw)) in &allocated {
        if inode.is_dir() {
            directories.insert(ino, read_directory(fs, inode, raw, &allocated)?);
        }
    }

    let mut parents = BTreeMap::from([(fs.sb.rootino, fs.sb.rootino)]);
    for (&ino, directory) in &directories {
        let mut names = HashSet::new();
        for (_, entry) in &directory.entries {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let name = if fs.sb.has_case_insensitive_dirs() {
                entry.name.iter().map(u8::to_ascii_lowercase).collect()
            } else {
                entry.name.clone()
            };
            if !dir::entry_name_is_valid(&entry.name) || !names.insert(name) {
                return Err(refuse(format!("inode {ino}: invalid or duplicate name")));
            }
            if allocated[&entry.ino].0.is_dir() && parents.insert(entry.ino, ino).is_some() {
                return Err(refuse(format!(
                    "directory {} has multiple parents or a cycle",
                    entry.ino
                )));
            }
        }
    }

    let mut patches = BTreeMap::new();
    for (&ino, directory) in &mut directories {
        let parent = *parents
            .get(&ino)
            .ok_or_else(|| refuse(format!("directory {ino} has no unambiguous parent")))?;
        fix_parent(directory, parent)?;
        if directory.inode.format == Format::Local {
            let (start, end) = directory.inode.data_fork_range(fs.sb.inodesize as usize);
            dir::read_short_form(&directory.inode, &directory.raw[start..end], &fs.sb)?;
            stamp(&mut directory.raw, crate::inode::offsets::CRC);
            add_patch(
                &mut patches,
                fs.inode_offset(ino)?,
                &directory.original,
                &directory.raw,
            )?;
        } else {
            repair_index(fs, directory)?;
            for block in directory.blocks.values_mut() {
                let data = matches!(
                    be32(&block.bytes, 0),
                    XFS_DIR3_DATA_MAGIC | XFS_DIR3_BLOCK_MAGIC | XFS_DIR3_FREE_MAGIC
                );
                stamp(
                    &mut block.bytes,
                    if data {
                        offsets::dir3_blk::CRC
                    } else {
                        offsets::da_blk::CRC
                    },
                );
                for (i, &at) in block.locations.iter().enumerate() {
                    let start = i * fs.sb.blocksize as usize;
                    let end = start + fs.sb.blocksize as usize;
                    add_patch(
                        &mut patches,
                        at,
                        &block.original[start..end],
                        &block.bytes[start..end],
                    )?;
                }
            }
        }
    }

    if patches.is_empty() {
        return Ok(patches);
    }
    let proposed = Arc::new(Proposed {
        device: fs.device.clone(),
        patches,
    });
    let preview = Filesystem::mount(proposed.clone())?;
    let checked = check::check(&preview);
    drop(preview);
    // Other rules own other damage; what this rule leaves must be a
    // complete scan with no directory finding in it.
    let left: Vec<&str> = checked
        .findings
        .iter()
        .filter(|f| CODES.contains(&f.code) || f.code.stops_the_walk())
        .take(4)
        .map(|f| f.what.as_str())
        .collect();
    if checked.scan != check::Scan::Complete || !left.is_empty() {
        return Err(refuse(left.join("; ")));
    }
    let proposed = Arc::try_unwrap(proposed).map_err(|_| refuse("the preview is still in use"))?;
    Ok(proposed.patches)
}

fn allocated_inodes(fs: &Filesystem) -> Result<BTreeMap<u64, (Inode, Vec<u8>)>> {
    let mut out = BTreeMap::new();
    for ag in 0..fs.sb.agcount {
        let agi = fs.read_agi(ag)?;
        let chunks = inode_btree::walk_from_agi(&fs.sb, &agi, Which::All, |b| {
            fs.read_fsblock((u64::from(ag) << fs.sb.agblklog) | u64::from(b))
        })?
        .ok_or_else(|| refuse("missing inode btree"))?;
        for chunk in chunks {
            for n in 0..INODES_PER_CHUNK {
                if chunk.exists(n) && !chunk.is_free(n) {
                    let ino = fs.sb.join_ino(ag, chunk.startino + u32::from(n));
                    let (inode, raw) = fs.read_inode_raw(ino)?;
                    if inode.mode == 0
                        || inode.file_type().is_none()
                        || out.insert(ino, (inode, raw)).is_some()
                    {
                        return Err(refuse(format!(
                            "allocated inode {ino} is invalid or duplicated"
                        )));
                    }
                }
            }
        }
    }
    Ok(out)
}

fn read_directory(
    fs: &Filesystem,
    inode: &Inode,
    raw: &[u8],
    allocated: &BTreeMap<u64, (Inode, Vec<u8>)>,
) -> Result<Directory> {
    let mut directory = Directory {
        inode: inode.clone(),
        raw: raw.to_vec(),
        original: raw.to_vec(),
        entries: Vec::new(),
        blocks: BTreeMap::new(),
        per: fs.sb.dirblocksize() / fs.sb.blocksize,
        parent: None,
    };
    if inode.format == Format::Local {
        let (start, end) = inode.data_fork_range(fs.sb.inodesize as usize);
        let size = usize::try_from(inode.size).map_err(|_| refuse("short-form size overflow"))?;
        let end = start
            .checked_add(size)
            .filter(|&e| e <= end)
            .ok_or_else(|| refuse("short-form size exceeds fork"))?;
        if end - start < XFS_DIR2_SF_HDR_SIZE_4 {
            return Err(refuse("truncated short-form directory"));
        }
        let width = if raw[start + offsets::sf_hdr::I8COUNT] == 0 {
            4
        } else {
            8
        };
        directory.parent = Some((None, start + offsets::sf_hdr::PARENT, width));
        let mut at = start + offsets::sf_hdr::PARENT + width;
        for _ in 0..raw[start + offsets::sf_hdr::COUNT] {
            if at + offsets::sf_entry::NAME > end {
                return Err(refuse("truncated short-form entry"));
            }
            let name_start = at + offsets::sf_entry::NAME;
            let name_end = name_start + raw[at] as usize;
            let next = name_end + 1 + width;
            if next > end {
                return Err(refuse("short-form entry runs past fork"));
            }
            if !dir::entry_name_is_valid(&raw[name_start..name_end]) {
                return Err(refuse("invalid short-form entry name"));
            }
            let ino = if width == 4 {
                u64::from(be32(raw, name_end + 1))
            } else {
                be64(raw, name_end + 1)
            };
            let target = allocated.get(&ino).ok_or_else(|| {
                refuse(format!(
                    "inode {}: entry {:?} has no allocated target {ino}",
                    inode.ino,
                    String::from_utf8_lossy(&raw[name_start..name_end])
                ))
            })?;
            let ftype = target.0.file_type();
            directory.raw[name_end] = dir::ftype_to_raw(ftype);
            directory.entries.push((
                0,
                DirEntry {
                    name: raw[name_start..name_end].to_vec(),
                    ino,
                    ftype,
                    offset: u32::from(be16(raw, at + offsets::sf_entry::OFFSET)),
                },
            ));
            at = next;
        }
        if at != end {
            return Err(refuse(
                "short-form count does not describe the complete fork",
            ));
        }
        return Ok(directory);
    }

    let extents = fs.data_extents(inode, raw)?;
    let per = u64::from(fs.sb.dirblocksize()) / u64::from(fs.sb.blocksize);
    let mut block_numbers = BTreeSet::new();
    for extent in &extents {
        if extent.is_unwritten() {
            return Err(refuse("directory has unwritten extents"));
        }
        for fb in extent.startoff..extent.end_offset() {
            let db =
                u32::try_from(fb / per).map_err(|_| refuse("directory block offset overflow"))?;
            block_numbers.insert(db);
        }
    }
    for db in block_numbers {
        let mut locations = Vec::new();
        let mut bytes = vec![0; fs.sb.dirblocksize() as usize];
        for i in 0..per {
            let fb = u64::from(db) * per + i;
            let extent = crate::extent::lookup(&extents, fb)
                .ok_or_else(|| refuse("incomplete directory block mapping"))?;
            let phys = extent
                .map(fb)
                .ok_or_else(|| refuse("invalid directory extent"))?;
            let at = fs.block_offset(phys);
            locations.push(at);
            let start = i as usize * fs.sb.blocksize as usize;
            fs.device
                .read_at(at, &mut bytes[start..start + fs.sb.blocksize as usize])?;
        }
        let original = bytes.clone();
        let magic = be32(&bytes, 0);
        if matches!(
            magic,
            XFS_DIR3_DATA_MAGIC | XFS_DIR3_BLOCK_MAGIC | XFS_DIR3_FREE_MAGIC
        ) {
            dir::verify_data_block(&bytes, &fs.sb, locations[0] / 512, inode.ino)?;
            if magic != XFS_DIR3_FREE_MAGIC {
                let end = if magic == XFS_DIR3_BLOCK_MAGIC {
                    let tail = bytes.len() - XFS_DIR2_BLOCK_TAIL_SIZE;
                    let count = be32(&bytes, tail) as usize;
                    tail.checked_sub(
                        count
                            .checked_mul(XFS_DIR2_LEAF_ENTRY_SIZE)
                            .ok_or_else(|| refuse("block index count overflow"))?,
                    )
                    .filter(|&end| end >= XFS_DIR3_DATA_HDR_SIZE)
                    .ok_or_else(|| refuse("block index overlaps data header"))?
                } else {
                    bytes.len()
                };
                scan_data(&mut directory, db, &mut bytes, end, allocated)?;
            }
        } else if matches!(
            be16(&bytes, offsets::da_blk::MAGIC),
            XFS_DIR3_LEAF1_MAGIC | XFS_DIR3_LEAFN_MAGIC | XFS_DA3_NODE_MAGIC
        ) {
            dir::verify_da_block(&bytes, &fs.sb, locations[0] / 512, inode.ino)?;
        } else {
            return Err(refuse(format!(
                "inode {}: unknown directory block {db}",
                inode.ino
            )));
        }
        directory.blocks.insert(
            db,
            Block {
                bytes,
                original,
                locations,
            },
        );
    }
    Ok(directory)
}

fn scan_data(
    directory: &mut Directory,
    db: u32,
    bytes: &mut [u8],
    end: usize,
    allocated: &BTreeMap<u64, (Inode, Vec<u8>)>,
) -> Result<()> {
    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    while at < end {
        if end - at < XFS_DIR2_DATA_ALIGN {
            return Err(refuse("truncated data record"));
        }
        if be16(bytes, at) == XFS_DIR2_DATA_FREE_TAG {
            let len = be16(bytes, at + offsets::data_unused::LENGTH) as usize;
            if len < XFS_DIR2_DATA_ALIGN
                || !len.is_multiple_of(XFS_DIR2_DATA_ALIGN)
                || len > end - at
            {
                return Err(refuse("invalid unused data record"));
            }
            bytes[at + len - 2..at + len].copy_from_slice(&(at as u16).to_be_bytes());
            at += len;
            continue;
        }
        if end - at < DATA_ENTRY_MIN_SIZE {
            return Err(refuse("truncated directory entry"));
        }
        let len = bytes[at + offsets::data_entry::NAMELEN] as usize;
        let size = crate::dir_block::entry_size(len);
        if len == 0 || size > end - at {
            return Err(refuse("invalid directory name length"));
        }
        let name_at = at + offsets::data_entry::NAME;
        let name = bytes[name_at..name_at + len].to_vec();
        let mut ino = be64(bytes, at);
        let ftype = if name == b"." {
            ino = directory.inode.ino;
            bytes[at..at + 8].copy_from_slice(&ino.to_be_bytes());
            Some(crate::inode::FileType::Directory)
        } else if name == b".." {
            if directory.parent.replace((Some(db), at, 8)).is_some() {
                return Err(refuse("duplicate parent entry"));
            }
            Some(crate::inode::FileType::Directory)
        } else {
            allocated
                .get(&ino)
                .ok_or_else(|| {
                    refuse(format!(
                        "inode {}: entry {:?} has no allocated target {ino}",
                        directory.inode.ino,
                        String::from_utf8_lossy(&name)
                    ))
                })?
                .0
                .file_type()
        };
        bytes[name_at + len] = dir::ftype_to_raw(ftype);
        bytes[at + size - 2..at + size].copy_from_slice(&(at as u16).to_be_bytes());
        directory.entries.push((
            db,
            DirEntry {
                name,
                ino,
                ftype,
                offset: at as u32,
            },
        ));
        at += size;
    }
    Ok(())
}

fn fix_parent(directory: &mut Directory, parent: u64) -> Result<()> {
    let (block, at, width) = directory
        .parent
        .ok_or_else(|| refuse("directory has no parent entry"))?;
    if block.is_none() {
        if width == 4 && parent > u64::from(u32::MAX) {
            return Err(refuse("parent needs a wider short-form fork"));
        }
        let mut wide = usize::from(parent > u64::from(u32::MAX));
        wide += directory
            .entries
            .iter()
            .filter(|(_, e)| e.ino > u64::from(u32::MAX))
            .count();
        // A nonzero i8count selects the width for the entire fork. Changing
        // its value is safe only if that width remains the same.
        if (wide == 0) != (width == 4) {
            return Err(refuse("short-form inode width is ambiguous"));
        }
        let count =
            u8::try_from(wide).map_err(|_| refuse("short-form wide inode count overflow"))?;
        let (fork_start, _) = directory.inode.data_fork_range(directory.raw.len());
        directory.raw[fork_start + offsets::sf_hdr::I8COUNT] = count;
    } else {
        let dots = directory
            .entries
            .iter()
            .filter(|(_, e)| e.name == b".")
            .count();
        if dots != 1 {
            return Err(refuse("directory has no unique dot entry"));
        }
    }
    let bytes = if let Some(db) = block {
        &mut directory
            .blocks
            .get_mut(&db)
            .expect("parent block read")
            .bytes
    } else {
        &mut directory.raw
    };
    if width == 4 {
        bytes[at..at + 4].copy_from_slice(&(parent as u32).to_be_bytes());
    } else {
        bytes[at..at + 8].copy_from_slice(&parent.to_be_bytes());
    }
    Ok(())
}

fn wanted_index(fs: &Filesystem, directory: &Directory) -> Result<Vec<(u32, u32)>> {
    let mut records = Vec::new();
    for (db, entry) in &directory.entries {
        let byte = u64::from(*db) * u64::from(fs.sb.dirblocksize()) + u64::from(entry.offset);
        let address = u32::try_from(byte / XFS_DIR2_DATA_ALIGN as u64)
            .map_err(|_| refuse("directory entry address overflow"))?;
        records.push((crate::dir_block::hash_for(&fs.sb, &entry.name), address));
    }
    records.sort_unstable();
    Ok(records)
}

fn records(bytes: &[u8], start: usize, count: usize) -> Result<Vec<(u32, u32)>> {
    if count > bytes.len().saturating_sub(start) / XFS_DIR2_LEAF_ENTRY_SIZE {
        return Err(refuse("index records overrun block"));
    }
    Ok((0..count)
        .map(|i| {
            let at = start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
            (
                be32(bytes, at),
                be32(bytes, at + offsets::leaf_entry::ADDRESS),
            )
        })
        .collect())
}

fn write_records(
    bytes: &mut [u8],
    start: usize,
    count: usize,
    wanted: &[(u32, u32)],
) -> Result<()> {
    if wanted.len() > count || count > bytes.len().saturating_sub(start) / XFS_DIR2_LEAF_ENTRY_SIZE
    {
        return Err(refuse("data entries do not fit existing index slots"));
    }
    let stale = count - wanted.len();
    for i in 0..count {
        let (hash, address) = if i < stale { (0, 0) } else { wanted[i - stale] };
        let at = start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        bytes[at..at + 4].copy_from_slice(&hash.to_be_bytes());
        bytes[at + 4..at + 8].copy_from_slice(&address.to_be_bytes());
    }
    Ok(())
}

fn repair_index(fs: &Filesystem, directory: &mut Directory) -> Result<()> {
    let wanted = wanted_index(fs, directory)?;
    let first = directory
        .blocks
        .get_mut(&0)
        .ok_or_else(|| refuse("directory has no first data block"))?;
    if be32(&first.bytes, 0) == XFS_DIR3_BLOCK_MAGIC {
        if directory.blocks.len() != 1 {
            return Err(refuse("block-form directory has other blocks"));
        }
        let first = directory.blocks.get_mut(&0).expect("first block");
        let tail = first.bytes.len() - XFS_DIR2_BLOCK_TAIL_SIZE;
        let count = be32(&first.bytes, tail) as usize;
        let start = tail - count * XFS_DIR2_LEAF_ENTRY_SIZE;
        let original = records(&first.bytes, start, count)?;
        if !index_agrees(&original, &wanted) {
            write_records(&mut first.bytes, start, count, &wanted)?;
        }
        let stale = u32::try_from(
            count
                .checked_sub(wanted.len())
                .ok_or_else(|| refuse("missing block index slots"))?,
        )
        .map_err(|_| refuse("stale count overflow"))?;
        first.bytes[tail + 4..tail + 8].copy_from_slice(&stale.to_be_bytes());
        dir::parse_block_form(&first.bytes, &fs.sb)?;
        return Ok(());
    }

    let root = u32::try_from((1u64 << 35) / u64::from(fs.sb.dirblocksize()))
        .expect("leaf region fits u32");
    let mut visited = HashSet::new();
    let mut leaves = Vec::new();
    let mut nodes = Vec::new();
    visit_index(directory, root, None, &mut visited, &mut leaves, &mut nodes)?;
    for (&db, block) in &directory.blocks {
        if db >= root && be32(&block.bytes, 0) != XFS_DIR3_FREE_MAGIC && !visited.contains(&db) {
            return Err(refuse("unreachable directory index block"));
        }
    }
    let counts_at = offsets::da_counts(XFS_DIR3_LEAF_HDR_SIZE, true);
    let mut all = Vec::new();
    let mut counts = Vec::new();
    let mut live_counts = Vec::new();
    for (i, &db) in leaves.iter().enumerate() {
        let block = &directory.blocks[&db].bytes;
        // Sibling pointers count filesystem blocks.
        let per = directory.per;
        let expected_back = if i == 0 { 0 } else { leaves[i - 1] * per };
        let expected_forw = leaves.get(i + 1).map_or(0, |&db| db * per);
        if be32(block, offsets::da_blk::BACK) != expected_back
            || be32(block, offsets::da_blk::FORW) != expected_forw
        {
            return Err(refuse("leaf chain does not agree with index topology"));
        }
        let count = be16(block, counts_at) as usize;
        if count == 0 {
            return Err(refuse("empty index leaf"));
        }
        let current = records(block, XFS_DIR3_LEAF_HDR_SIZE, count)?;
        counts.push(count);
        live_counts.push(current.iter().filter(|r| r.1 != 0).count());
        all.extend(current);
    }
    if !index_agrees(&all, &wanted) {
        if wanted.len() < leaves.len() || counts.iter().sum::<usize>() < wanted.len() {
            return Err(refuse("data entries do not fit existing leaves"));
        }
        if live_counts.iter().sum::<usize>() != wanted.len() || live_counts.contains(&0) {
            let mut remaining = wanted.len();
            for i in 0..counts.len() {
                let take = counts[i].min(remaining - (counts.len() - i - 1));
                if take == 0 {
                    return Err(refuse("empty index leaf"));
                }
                live_counts[i] = take;
                remaining -= take;
            }
        }
        let mut at = 0;
        for (i, &db) in leaves.iter().enumerate() {
            let next = at + live_counts[i];
            write_records(
                &mut directory.blocks.get_mut(&db).expect("leaf").bytes,
                XFS_DIR3_LEAF_HDR_SIZE,
                counts[i],
                &wanted[at..next],
            )?;
            at = next;
        }
    }
    for (i, &db) in leaves.iter().enumerate() {
        let block = &mut directory.blocks.get_mut(&db).expect("leaf").bytes;
        let stale = counts[i] - live_counts[i];
        block[counts_at + 2..counts_at + 4].copy_from_slice(&(stale as u16).to_be_bytes());
        dir::parse_leaf(block, &fs.sb)?;
    }
    // DFS lists nodes after their children, so child bounds are final here.
    for db in nodes {
        let block = &directory.blocks[&db].bytes;
        let count = be16(block, offsets::da_counts(XFS_DA3_NODE_HDR_SIZE, true)) as usize;
        let children = (0..count)
            .map(|i| {
                directory.block_of(be32(
                    block,
                    XFS_DA3_NODE_HDR_SIZE
                        + i * XFS_DA_NODE_ENTRY_SIZE
                        + offsets::node_entry::BEFORE,
                ))
            })
            .collect::<Result<Vec<u32>>>()?;
        let mut bounds = Vec::new();
        for child in children {
            let bytes = &directory.blocks[&child].bytes;
            let node = be16(bytes, offsets::da_blk::MAGIC) == XFS_DA3_NODE_MAGIC;
            let hdr = if node {
                XFS_DA3_NODE_HDR_SIZE
            } else {
                XFS_DIR3_LEAF_HDR_SIZE
            };
            let n = be16(bytes, offsets::da_counts(hdr, true)) as usize;
            bounds.push(be32(bytes, hdr + (n - 1) * XFS_DIR2_LEAF_ENTRY_SIZE));
        }
        let block = &mut directory.blocks.get_mut(&db).expect("node").bytes;
        for (i, bound) in bounds.into_iter().enumerate() {
            let at = XFS_DA3_NODE_HDR_SIZE + i * XFS_DA_NODE_ENTRY_SIZE;
            block[at..at + 4].copy_from_slice(&bound.to_be_bytes());
        }
        dir::parse_node(block, &fs.sb)?;
    }
    // Data parsers now see repaired tags/types, and block-form indexes too.
    for (&db, block) in &directory.blocks {
        if db < root {
            dir::parse_data_block(&block.bytes, &fs.sb)?;
        }
    }
    Ok(())
}

fn index_agrees(records: &[(u32, u32)], wanted: &[(u32, u32)]) -> bool {
    if records.windows(2).any(|w| w[0].0 > w[1].0) {
        return false;
    }
    let mut live: Vec<_> = records.iter().copied().filter(|r| r.1 != 0).collect();
    live.sort_unstable();
    live == wanted
}

fn visit_index(
    directory: &Directory,
    db: u32,
    level: Option<u16>,
    visited: &mut HashSet<u32>,
    leaves: &mut Vec<u32>,
    nodes: &mut Vec<u32>,
) -> Result<()> {
    if !visited.insert(db) {
        return Err(refuse("index cycle or duplicate child"));
    }
    let block = &directory
        .blocks
        .get(&db)
        .ok_or_else(|| refuse("index child is a hole"))?
        .bytes;
    let magic = be16(block, offsets::da_blk::MAGIC);
    if matches!(magic, XFS_DIR3_LEAF1_MAGIC | XFS_DIR3_LEAFN_MAGIC) {
        if level.is_some_and(|l| l != 0) || (magic == XFS_DIR3_LEAF1_MAGIC && level.is_some()) {
            return Err(refuse("leaf is at the wrong level"));
        }
        leaves.push(db);
        return Ok(());
    }
    if magic != XFS_DA3_NODE_MAGIC {
        return Err(refuse("index child is not a node or leaf"));
    }
    let counts = offsets::da_counts(XFS_DA3_NODE_HDR_SIZE, true);
    let n = be16(block, counts) as usize;
    let height = be16(block, counts + 2);
    if height == 0
        || height > dir::MAX_SUPPORTED_NODE_LEVEL
        || level.is_some_and(|l| l != height)
        || n == 0
        || n > (block.len() - XFS_DA3_NODE_HDR_SIZE) / XFS_DA_NODE_ENTRY_SIZE
    {
        return Err(refuse("invalid node height or count"));
    }
    for i in 0..n {
        let child = directory.block_of(be32(
            block,
            XFS_DA3_NODE_HDR_SIZE + i * XFS_DA_NODE_ENTRY_SIZE + offsets::node_entry::BEFORE,
        ))?;
        visit_index(directory, child, Some(height - 1), visited, leaves, nodes)?;
    }
    nodes.push(db);
    Ok(())
}

fn stamp(bytes: &mut [u8], crc_at: usize) {
    let crc = crc32c_with_zeroed_crc(bytes, crc_at);
    bytes[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
}

fn add_patch(
    patches: &mut BTreeMap<u64, Patch>,
    at: u64,
    before: &[u8],
    after: &[u8],
) -> Result<()> {
    if before == after {
        return Ok(());
    }
    let end = at
        .checked_add(after.len() as u64)
        .ok_or_else(|| refuse("patch offset overflow"))?;
    if patches
        .iter()
        .any(|(&start, p)| start < end && start + p.after.len() as u64 > at)
    {
        return Err(refuse("metadata buffers overlap"));
    }
    patches.insert(
        at,
        Patch {
            after: after.to_vec(),
        },
    );
    Ok(())
}

struct Proposed {
    device: Arc<dyn BlockRead>,
    patches: BTreeMap<u64, Patch>,
}

impl BlockRead for Proposed {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.device.read_at(offset, buf)?;
        let end = offset + buf.len() as u64;
        for (&at, patch) in self.patches.range(..end) {
            let start = at.max(offset);
            let stop = (at + patch.after.len() as u64).min(end);
            if start < stop {
                buf[(start - offset) as usize..(stop - offset) as usize]
                    .copy_from_slice(&patch.after[(start - at) as usize..(stop - at) as usize]);
            }
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.device.size_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_index_must_be_sorted_complete_and_unique() {
        let wanted = [(1, 8), (2, 16)];
        assert!(index_agrees(&[(0, 0), (1, 8), (2, 16)], &wanted));
        assert!(!index_agrees(&[(2, 16), (1, 8)], &wanted));
        assert!(!index_agrees(&[(1, 8), (1, 8)], &wanted));
        assert!(!index_agrees(&[(1, 8)], &wanted));
        assert!(!index_agrees(&[(1, 16), (2, 8)], &wanted));
    }

    #[test]
    fn stale_slots_are_rebuilt_before_live_records() {
        let mut bytes = vec![0; 32];
        write_records(&mut bytes, 0, 4, &[(1, 8), (2, 16)]).unwrap();
        assert_eq!(
            records(&bytes, 0, 4).unwrap(),
            [(0, 0), (0, 0), (1, 8), (2, 16)]
        );
        assert!(write_records(&mut bytes, 0, 1, &[(1, 8), (2, 16)]).is_err());
        assert!(records(&bytes, 0, 5).is_err());
    }

    #[test]
    fn unchanged_and_overlapping_buffers_are_not_written() {
        let mut patches = BTreeMap::new();
        add_patch(&mut patches, 4, &[1, 2], &[1, 2]).unwrap();
        assert!(patches.is_empty());
        add_patch(&mut patches, 4, &[1, 2], &[3, 4]).unwrap();
        assert!(add_patch(&mut patches, 5, &[0, 0], &[1, 1]).is_err());
        add_patch(&mut patches, 6, &[0], &[1]).unwrap();
    }
}
