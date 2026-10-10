//! Offline repair of unambiguous inode allocation and link counts (#393).
//!
//! Existing, checksum-valid inode slots and a complete directory traversal
//! determine allocation and links. No inode, directory entry, extent or
//! sparse-chunk hole is invented or discarded. Both inode trees and their
//! associated free counts follow that census. An orphan, multiple directory
//! parents, damaged identity, or any remaining checker finding refuses the
//! entire plan before a byte is written.
//!
//! It is a repair-planner rule ([`InodeAllocation`]): it proposes every
//! change, and the plan's own preconditions (exclusive, unmounted, clean
//! log) and [`crate::repair::apply`] do the rest. The rule first applies
//! its proposal to a view of the volume and checks that view, and refuses
//! if anything is still wrong.

use crate::ag_btree::{self, offsets as bt};
use crate::check::{Code, Report as Check};
use crate::endian::{be16, be32, be64};
use crate::inode::{self, offsets as di, Inode};
use crate::inode_btree::{self, InodeChunk, Trees, Which, INODES_PER_CHUNK};
use crate::repair::{Proposal, Rule};
use crate::{Error, Filesystem, Result};
use fs_core::BlockRead;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

struct Patch {
    after: Vec<u8>,
}

struct Plan {
    original: Arc<dyn BlockRead>,
    view: Arc<crate::overlay::Overlay>,
    patches: BTreeMap<u64, Patch>,
}

impl Plan {
    fn new(original: Arc<dyn BlockRead>) -> Self {
        Self {
            view: Arc::new(crate::overlay::Overlay::new(original.clone())),
            original,
            patches: BTreeMap::new(),
        }
    }

    fn put(&mut self, at: u64, after: Vec<u8>) -> Result<()> {
        let mut before = vec![0; after.len()];
        self.original.read_at(at, &mut before)?;
        self.view.wrote(at, &after);
        if before == after {
            self.patches.remove(&at);
        } else {
            self.patches.insert(at, Patch { after });
        }
        Ok(())
    }
}

fn refused(why: &str) -> Error {
    Error::UnsupportedFeature(format!("inode repair refused: {why}"))
}

/// Every code [`InodeAllocation`] repairs.
pub(crate) const CODES: &[Code] = &[
    Code::InodeFreeInUse,
    Code::InodeAllocatedUnused,
    Code::InodeNlink,
    Code::InobtChunkCount,
    Code::FinobtMismatch,
];

/// Inode allocation bits, their mirrored free counts, and link counts,
/// repaired from the inodes themselves and a complete directory traversal
/// (#393).
///
/// Every live non-metadata inode must be reached by an unambiguous
/// directory traversal. No other kind of damage is repaired, and any
/// other finding refuses the plan: the rule applies its proposal to a
/// view of the volume, checks the view, and refuses if anything is left.
pub struct InodeAllocation;

impl Rule for InodeAllocation {
    fn name(&self) -> &'static str {
        "inode-allocation"
    }

    fn repairs(&self) -> &'static [Code] {
        CODES
    }

    fn propose(&self, fs: &Filesystem, report: &Check, proposal: &mut Proposal) -> Result<()> {
        if !report.findings.iter().any(|f| CODES.contains(&f.code)) {
            return Ok(());
        }
        for (at, patch, code) in plan_changes(fs)? {
            proposal.put(
                at,
                patch.after,
                code,
                match code {
                    Code::InodeNlink => "set a link count to the entries that reach the inode",
                    _ => "set inode allocation to the inodes in use, and the counts that follow",
                },
            )?;
        }
        Ok(())
    }
}

/// Every change the repair makes, with the code it repairs.
fn plan_changes(fs: &Filesystem) -> Result<Vec<(u64, Patch, Code)>> {
    let device = fs.device.clone();
    let sb = fs.superblock();
    if !sb.is_v5() {
        return Err(refused(
            "only checksum-protected v5 inode slots are repairable",
        ));
    }
    if sb.rblocks != 0 || sb.qflags != 0 {
        return Err(refused(
            "realtime or quota inode ownership needs another repairer",
        ));
    }
    let mut plan = Plan::new(device.clone());
    let metadata: HashSet<u64> = [sb.rbmino, sb.rsumino, sb.uquotino, sb.gquotino, sb.pquotino]
        .into_iter()
        .filter(|&i| i != 0 && i != u64::MAX)
        .collect();
    let mut slots = BTreeMap::new();
    let mut groups = Vec::new();
    let mut old_ifree = 0u64;

    // Validate the tree headers, addresses, UUIDs, CRCs and chunk geometry
    // before using their slots. Only the free-count field is relaxed here.
    for ag in 0..sb.agcount {
        let agi = fs.read_agi(ag)?;
        let start = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize);
        let (records, _) = ag_btree::walk_blocks(
            sb,
            inode_btree::shape(Which::All, true),
            ag,
            agi.root,
            agi.level,
            |b| {
                let mut raw = vec![0; sb.blocksize as usize];
                fs.device()
                    .read_at(start + u64::from(b) * u64::from(sb.blocksize), &mut raw)?;
                Ok(raw)
            },
            |raw, at| relaxed_record(raw, at, sb.has_sparse_inodes()),
        )?;
        let records = records.into_iter().collect::<Result<Vec<_>>>()?;
        let mut previous = None;
        for record in &records {
            let chunk = &record.chunk;
            let last = u64::from(chunk.startino) + u64::from(INODES_PER_CHUNK) - 1;
            let length =
                (sb.dblocks - u64::from(ag) * u64::from(sb.agblocks)).min(u64::from(sb.agblocks));
            // A chunk starts on a block aligned as the superblock says
            // (`sb_spino_align` under sparse inodes, else `sb_inoalignmt`),
            // not at an inode number that is a multiple of 64: a plain
            // chunk at inode 96 is block 12, aligned and legitimate.
            let align = if sb.has_sparse_inodes() {
                sb.spino_align
            } else {
                sb.inoalignmt
            };
            let block = chunk.startino >> sb.inopblog;
            if !chunk.startino.is_multiple_of(u32::from(sb.inopblock))
                || (align != 0 && !block.is_multiple_of(align))
                || u64::from(chunk.startino >> sb.inopblog)
                    <= u64::from(crate::agfl::last_header_block(sb))
                || last >> sb.inopblog >= length
                || previous.is_some_and(|end| u64::from(chunk.startino) <= end)
                || (0..INODES_PER_CHUNK).filter(|&n| chunk.exists(n)).count()
                    != usize::from(chunk.count)
            {
                return Err(refused("inode chunks overlap or have uncertain geometry"));
            }
            previous = Some(last);
            old_ifree += u64::from(record.declared_free);
            for n in 0..INODES_PER_CHUNK {
                if !chunk.exists(n) {
                    continue;
                }
                let ino = sb.join_ino(ag, chunk.startino + u32::from(n));
                let (inode, raw) = fs.read_inode_raw(ino)?;
                if inode.mode == 0 {
                    inode::verify_free_slot(&raw, sb, ino)?;
                    if inode.nblocks != 0
                        || inode.nextents != 0
                        || inode.anextents != 0
                        || inode.nlink != 0
                        || inode.next_unlinked != u32::MAX
                    {
                        return Err(refused("an unused inode still claims ownership"));
                    }
                } else if inode.file_type().is_none() || inode.next_unlinked != u32::MAX {
                    return Err(refused("an inode has an uncertain type or unlinked owner"));
                }
                if metadata.contains(&ino) && (inode.mode == 0 || chunk.is_free(n)) {
                    return Err(refused(
                        "metadata inode allocation cannot be inferred from directories",
                    ));
                }
                slots.insert(ino, (inode, raw));
            }
        }
        groups.push((ag, agi, records));
    }

    let links = namespace(fs, &slots)?;
    for (&ino, (inode, _)) in &slots {
        if inode.mode != 0 && !links.contains_key(&ino) && !metadata.contains(&ino) {
            return Err(refused(&format!(
                "live inode {ino} has no unambiguous directory owner"
            )));
        }
    }

    let mut link_fixes: HashSet<u64> = HashSet::new();
    let mut new_ifree = 0u64;
    for (ag, agi, records) in groups {
        let mut chunks = Vec::new();
        let old_free: u64 = records.iter().map(|r| u64::from(r.declared_free)).sum();
        for record in records {
            let mut chunk = record.chunk;
            for n in 0..INODES_PER_CHUNK {
                if !chunk.exists(n) {
                    continue;
                }
                let ino = sb.join_ino(ag, chunk.startino + u32::from(n));
                let want_free = slots[&ino].0.mode == 0;
                if want_free != chunk.is_free(n) {
                    chunk.free ^= 1u64 << n;
                }
            }
            chunk.freecount = (0..INODES_PER_CHUNK)
                .filter(|&n| chunk.exists(n) && chunk.is_free(n))
                .count() as u8;
            let mut raw = vec![0; sb.blocksize as usize];
            plan.view.read_at(record.address, &mut raw)?;
            inode_btree::encode(&mut raw, record.at, &chunk, sb.has_sparse_inodes());
            crate::group_write::restamp_crc(&mut raw, bt::CRC);
            plan.put(record.address, raw)?;
            chunks.push(chunk);
        }
        let count = chunks
            .iter()
            .try_fold(0u32, |n, c| n.checked_add(u32::from(c.count)))
            .ok_or_else(|| refused("AG inode count overflow"))?;
        let free = chunks
            .iter()
            .try_fold(0u32, |n, c| n.checked_add(u32::from(c.freecount)))
            .ok_or_else(|| refused("AG free-inode count overflow"))?;
        // Only counters corroborated by either side of the allocation fault
        // follow it. Independent counter corruption belongs to another repair.
        if agi.count != count || ![old_free, u64::from(free)].contains(&u64::from(agi.freecount)) {
            return Err(refused(
                "an AGI counter is unrelated to the allocation inconsistency",
            ));
        }
        new_ifree += u64::from(free);

        let desired: Vec<_> = chunks
            .iter()
            .copied()
            .filter(|c| c.freecount != 0)
            .collect();
        let start = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize);
        let finobt = if sb.has_finobt() {
            let (records, _) = ag_btree::walk_blocks(
                sb,
                inode_btree::shape(Which::WithFreeInodes, true),
                ag,
                agi.free_root,
                agi.free_level,
                |b| {
                    let mut raw = vec![0; sb.blocksize as usize];
                    plan.view
                        .read_at(start + u64::from(b) * u64::from(sb.blocksize), &mut raw)?;
                    Ok(raw)
                },
                |raw, at| relaxed_record(raw, at, sb.has_sparse_inodes()),
            )?;
            records.into_iter().collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        if sb.has_finobt()
            && finobt
                .iter()
                .map(|r| r.chunk.startino)
                .ne(desired.iter().map(|c| c.startino))
        {
            // Membership changes need the existing writer, including its AGFL
            // accounting. Ordinary repairs preserve the original tree layout.
            let view = Filesystem::mount(plan.view.clone())?;
            let mut trees = Trees::open(sb, view.device(), ag)?;
            *trees.chunks_mut() = chunks;
            trees.set_counts(count, free, None);
            for item in trees.into_items()? {
                plan.put(item.blkno() * 512, item.image_as_written())?;
            }
        } else {
            for (record, chunk) in finobt.iter().zip(&desired) {
                let mut raw = vec![0; sb.blocksize as usize];
                plan.view.read_at(record.address, &mut raw)?;
                inode_btree::encode(&mut raw, record.at, chunk, sb.has_sparse_inodes());
                crate::group_write::restamp_crc(&mut raw, bt::CRC);
                plan.put(record.address, raw)?;
            }
            if agi.freecount != free {
                let at = start + 2 * u64::from(sb.sectsize);
                let mut raw = vec![0; sb.sectsize as usize];
                plan.view.read_at(at, &mut raw)?;
                let off = crate::ag::offsets::agi::FREECOUNT;
                raw[off..off + 4].copy_from_slice(&free.to_be_bytes());
                crate::group_write::restamp_crc(&mut raw, crate::ag::offsets::agi::CRC);
                plan.put(at, raw)?;
            }
        }
    }
    if ![old_ifree, new_ifree].contains(&sb.ifree) {
        return Err(refused(
            "the superblock free-inode count is unrelated to the allocation inconsistency",
        ));
    }
    if sb.ifree != new_ifree {
        let mut raw = vec![0; sb.sectsize as usize];
        plan.view.read_at(0, &mut raw)?;
        let at = crate::superblock::offsets::IFREE;
        raw[at..at + 8].copy_from_slice(&new_ifree.to_be_bytes());
        crate::group_write::restamp_crc(&mut raw, crate::superblock::SB_CRC_OFFSET);
        plan.put(0, raw)?;
    }
    for (ino, want) in links {
        let (inode, raw) = &slots[&ino];
        if inode.nlink != want {
            let mut raw = raw.clone();
            raw[di::NLINK..di::NLINK + 4].copy_from_slice(&want.to_be_bytes());
            crate::group_write::restamp_crc(&mut raw, di::CRC);
            let at = fs.inode_offset(ino)?;
            plan.put(at, raw)?;
            link_fixes.insert(at);
        }
    }
    let candidate = Filesystem::mount(plan.view.clone())?;
    let checked = crate::check::check(&candidate);
    if let Some(finding) = checked.findings.first() {
        return Err(refused(&format!(
            "the proposed repair still has damage: {}",
            finding.what
        )));
    }
    Ok(plan
        .patches
        .into_iter()
        .map(|(at, patch)| {
            let code = if link_fixes.contains(&at) {
                Code::InodeNlink
            } else {
                Code::InodeFreeInUse
            };
            (at, patch, code)
        })
        .collect())
}

struct Record {
    chunk: InodeChunk,
    declared_free: u32,
    address: u64,
    at: usize,
}

/// Decode through the ordinary parser after normalising just its free count.
/// The source block has already passed the ordinary CRC/identity verifier.
fn relaxed_record(raw: &[u8], at: usize, sparse: bool) -> Result<Record> {
    let mut fixed = raw.to_vec();
    let mut probe = InodeChunk {
        startino: be32(raw, at),
        holemask: if sparse { be16(raw, at + 4) } else { 0 },
        count: if sparse {
            raw[at + 6]
        } else {
            INODES_PER_CHUNK
        },
        freecount: 0,
        free: be64(raw, at + 8),
    };
    let declared_free = if sparse {
        u32::from(raw[at + 7])
    } else {
        be32(raw, at + 4)
    };
    probe.freecount = (0..INODES_PER_CHUNK)
        .filter(|&n| probe.exists(n) && probe.is_free(n))
        .count() as u8;
    inode_btree::encode(&mut fixed, at, &probe, sparse);
    Ok(Record {
        chunk: inode_btree::record(&fixed, at, sparse)?,
        declared_free,
        address: be64(raw, bt::BLKNO) * 512,
        at,
    })
}

fn namespace(
    fs: &Filesystem,
    slots: &BTreeMap<u64, (Inode, Vec<u8>)>,
) -> Result<HashMap<u64, u32>> {
    let root = fs.superblock().rootino;
    let mut stack = vec![(root, root)];
    let mut dirs = HashSet::from([root]);
    let mut links: HashMap<u64, u32> = HashMap::new();
    while let Some((ino, parent)) = stack.pop() {
        let (inode, raw) = slots
            .get(&ino)
            .ok_or_else(|| refused("a directory is outside all inode chunks"))?;
        if !inode.is_dir() {
            return Err(refused("a directory entry has no directory inode"));
        }
        check_parent(fs, inode, raw, parent)?;
        let mut names = HashSet::new();
        links.insert(ino, 2);
        for entry in fs.read_dir(inode, raw)? {
            if entry.name.is_empty()
                || entry.name.contains(&0)
                || entry.name.contains(&b'/')
                || !names.insert(entry.name.clone())
            {
                return Err(refused("a directory has ambiguous entry names"));
            }
            if [
                fs.superblock().rbmino,
                fs.superblock().rsumino,
                fs.superblock().uquotino,
                fs.superblock().gquotino,
                fs.superblock().pquotino,
            ]
            .contains(&entry.ino)
            {
                return Err(refused("a directory references a metadata inode"));
            }
            let (target, _) = slots
                .get(&entry.ino)
                .ok_or_else(|| refused("an entry points outside all inode chunks"))?;
            if target.mode == 0 || entry.ftype.is_some_and(|t| Some(t) != target.file_type()) {
                return Err(refused(
                    "an entry's inode is unused or has a conflicting type",
                ));
            }
            if target.is_dir() {
                if !dirs.insert(entry.ino) {
                    return Err(refused("a directory has multiple parents or a cycle"));
                }
                let count = links
                    .get_mut(&ino)
                    .expect("the current directory is counted");
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| refused("directory link count overflow"))?;
                stack.push((entry.ino, ino));
            } else {
                let count = links.entry(entry.ino).or_default();
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| refused("file link count overflow"))?;
            }
        }
    }
    Ok(links)
}

fn check_parent(fs: &Filesystem, inode: &Inode, raw: &[u8], parent: u64) -> Result<()> {
    let sb = fs.superblock();
    if inode.format == inode::Format::Local {
        let (start, end) = inode.data_fork_range(sb.inodesize as usize);
        let dir = crate::dir::read_short_form(inode, &raw[start..end], sb)?;
        if dir.parent_ino != parent {
            return Err(refused(
                "a directory's recorded parent disagrees with traversal",
            ));
        }
    } else {
        let extents = fs.data_extents(inode, raw)?;
        let stride = u64::from(sb.dirblocksize()) / u64::from(sb.blocksize);
        // XFS places the leaf region at byte offset 1 << 35, as the
        // existing Filesystem::read_dir implementation does. Validate
        // every mapped data block: read_dir can omit an unparseable block,
        // which an ownership census must never treat as an empty directory.
        let limit = (1u64 << 35) / u64::from(sb.blocksize);
        let mut blocks = std::collections::BTreeSet::new();
        for extent in &extents {
            if extent.startoff >= limit {
                continue;
            }
            if extent.is_unwritten() {
                return Err(refused("directory data is unwritten"));
            }
            let end = extent.end_offset().min(limit);
            blocks.extend(extent.startoff / stride..end.div_ceil(stride));
        }
        if !blocks.contains(&0) {
            return Err(refused("a directory has no first data block"));
        }
        for b in blocks {
            let phys = extents
                .iter()
                .find_map(|e| e.map(b * stride))
                .ok_or_else(|| refused("a directory data block is incomplete"))?;
            // From the device, at the block the map gives, as `read_dir`
            // reads it: `read_at` reads regular files and refuses a
            // directory, which failed this rule on every volume with a
            // directory past short form.
            let mut block = vec![0; sb.dirblocksize() as usize];
            fs.device().read_at(sb.fsblock_offset(phys), &mut block)?;
            crate::dir::verify_data_block(&block, sb, sb.fsblock_offset(phys) / 512, inode.ino)?;
            let entries = crate::dir::parse_data_block(&block, sb)?;
            for (name, want) in [(b".".as_slice(), inode.ino), (b"..".as_slice(), parent)] {
                let dots: Vec<_> = entries.iter().filter(|e| e.name == name).collect();
                if (b == 0 && (dots.len() != 1 || dots[0].ino != want))
                    || (b != 0 && !dots.is_empty())
                {
                    return Err(refused("directory dot entries disagree with traversal"));
                }
            }
        }
    }
    Ok(())
}
