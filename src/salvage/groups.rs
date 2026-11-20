//! A group's inode and free-space B+trees, read for salvage.

use super::evidence::{Damage, DamageKind, Extent, Severity};
use super::inodes::{self, InodeCandidate, InodeScan};
use crate::ag::{agf_btree, Agf, Agi};
use crate::ag_btree;
use crate::alloc_btree::{self, FreeExtent, Order};
use crate::inode_btree::{self, InodeChunk, Which, INODES_PER_CHUNK};
use crate::superblock::Superblock;
use fs_core::BlockRead;

/// What a damage-tolerant walk of one of a group's trees reached.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeWalk<T> {
    pub ag: u32,
    /// Records from every leaf that could be used, in tree order where
    /// the tree is intact.
    pub records: Vec<T>,
    /// The tree's own blocks that were read and used.
    pub blocks: Vec<Extent>,
    /// Blocks that could not be used, and why. A leaf lost here takes its
    /// records with it, so an empty `damage` is what makes `records`
    /// complete.
    pub damage: Vec<Damage>,
}

fn walk<T>(
    device: &dyn BlockRead,
    sb: &Superblock,
    ag: u32,
    shape: ag_btree::Shape,
    root: u32,
    levels: u32,
    decode: impl Fn(&[u8], usize) -> Result<T, String>,
) -> TreeWalk<T> {
    let block = u64::from(sb.blocksize);
    let at = |agblock: u32| sb.ag_offset(ag) + u64::from(agblock) * block;
    let walked = ag_btree::walk_salvage(
        sb,
        shape,
        ag,
        root,
        levels,
        |agblock| {
            if agblock >= sb.agblocks {
                return None;
            }
            let mut buf = vec![0u8; block as usize];
            device.read_at(at(agblock), &mut buf).ok().map(|()| buf)
        },
        decode,
    );
    let structure = format!("{} {ag}", shape.name);
    TreeWalk {
        ag,
        records: walked.records,
        blocks: walked
            .blocks
            .iter()
            .map(|&b| Extent::new(at(b), block))
            .collect(),
        damage: walked
            .problems
            .into_iter()
            .map(|(agblock, kind, detail)| {
                let kind = if kind == DamageKind::Unreadable && agblock >= sb.agblocks {
                    DamageKind::Malformed
                } else {
                    kind
                };
                Damage::new(
                    kind,
                    Severity::Error,
                    Extent::new(at(agblock), block),
                    structure.clone(),
                    detail,
                )
            })
            .collect(),
    }
}

/// The inode chunks group `ag`'s inode B+tree records, from the root its
/// AGI names. Use [`super::ag_headers`] for the AGI, decoded past damage.
pub fn inode_chunks(
    device: &dyn BlockRead,
    sb: &Superblock,
    ag: u32,
    agi: &Agi,
) -> TreeWalk<InodeChunk> {
    let sparse = sb.has_sparse_inodes();
    walk(
        device,
        sb,
        ag,
        inode_btree::shape(Which::All, sb.is_v5()),
        agi.root,
        agi.level,
        |buf, at| inode_btree::record(buf, at, sparse).map_err(|e| e.to_string()),
    )
}

/// The free extents group `ag`'s by-block free-space B+tree records, from
/// the root its AGF names.
pub fn free_extents(
    device: &dyn BlockRead,
    sb: &Superblock,
    ag: u32,
    agf: &Agf,
) -> TreeWalk<FreeExtent> {
    walk(
        device,
        sb,
        ag,
        Order::ByBlock.shape(),
        agf.roots[agf_btree::BNO],
        agf.levels[agf_btree::BNO],
        |buf, at| Ok(alloc_btree::decode_free_extent(buf, at)),
    )
}

/// The bytes of the inodes a chunk holds.
pub fn chunk_location(sb: &Superblock, ag: u32, chunk: &InodeChunk) -> Extent {
    let first = sb.join_ino(ag, chunk.startino);
    let start = inodes::inode_location(sb, first).offset;
    Extent::new(start, u64::from(INODES_PER_CHUNK) * u64::from(sb.inodesize))
}

/// Scan the inodes of every chunk `chunks` names, passing each record to
/// `found` until it returns `false`.
///
/// Far less to read than [`inodes::find_inodes`] over a whole device, and
/// it finds the inodes the tree knows of. Run the device scan as well when
/// a tree is damaged: a lost leaf loses its chunks from this list.
pub fn find_inodes_in_chunks(
    device: &dyn BlockRead,
    sb: &Superblock,
    ag: u32,
    chunks: &[InodeChunk],
    found: &mut dyn FnMut(InodeCandidate) -> bool,
) -> InodeScan {
    let mut report = InodeScan::default();
    for chunk in chunks {
        let range = chunk_location(sb, ag, chunk);
        let part = inodes::find_inodes(device, sb, range, found);
        report.candidates += part.candidates;
        report.unreadable.extend(part.unreadable);
        if part.stopped_at.is_some() {
            report.stopped_at = part.stopped_at;
            return report;
        }
    }
    report
}
