//! Writing a data fork's extent map, inline or as a B+tree (#367).
//!
//! A directory in node form maps three spaces — data, index and free —
//! and every block in them can be anywhere, so its extents soon stop
//! fitting in the inode. XFS then keeps them in a block-map B+tree whose
//! root is the fork, and so must this driver, or a directory that grows
//! is refused the moment its map outgrows its inode.
//!
//! The map is laid out whole from its extent list, every time it
//! changes: the tree it had is given back and a new one built bottom up.
//! A tree here is a handful of blocks even for a very large directory —
//! a 4 KiB leaf holds 251 extents — so rebuilding it costs little next to
//! the directory blocks the same change logs.
//!
//! # Which form
//!
//! The kernel's own rule, both ways: an inode whose extents fit its fork
//! lists them there, and one whose extents do not keeps a tree. The
//! kernel refuses a tree whose extents would fit the fork as corrupt, so
//! the choice is not a matter of taste.
//!
//! # The root is logged in another shape than it is stored
//!
//! On disk the root is `xfs_bmdr_block`: a 4-byte header, keys, and the
//! pointers after room for as many keys as the fork could hold. In the
//! log it is the kernel's in-memory `xfs_btree_block`: the full block
//! header, then the keys, then the pointers straight after them, and
//! recovery converts it (`xfs_bmbt_to_bmdr`). Logging the on-disk shape
//! instead would have the kernel read the header as keys.
//!
//! # Every non-root block holds at least half
//!
//! `xfs_repair` refuses a block below its minimum below the first level,
//! so the extents are shared out evenly rather than packed: every block
//! of a level then holds within one of the same count, which is more than
//! half whenever there are two or more.

use crate::buf_write::BufferItem;
use crate::error::{Error, Result};
use crate::extent::Extent;
use crate::format::log_items::buf_log_format::buf_type::BLFT_BTREE;
use crate::format::log_items::inode_log_format::XFS_ILOG_DEXT;
use crate::fs::Filesystem;
use crate::group_write::{changed_chunks, Allocations};
use crate::inode::{Format, Inode};

/// `XFS_ILOG_DBROOT` — the item logs the data fork's B+tree root.
pub(crate) const XFS_ILOG_DBROOT: u32 = 0x08;

/// `XFS_BMBT_BLOCK_LEN` on v5: the long-form header.
const HEADER: usize = 72;
/// A record, or a key and its pointer: 16 bytes either way.
const SLOT: usize = 16;
/// The in-inode root's header: level and count.
const ROOT_HEADER: usize = 4;
/// `NULLFSBLOCK`: no sibling.
const NULL_BLOCK: u64 = u64::MAX;

/// A data fork laid out from its extents.
pub(crate) struct ForkMap {
    /// The fork as it is stored in the inode.
    pub fork: Vec<u8>,
    /// The fork as the inode item logs it.
    pub logged: Vec<u8>,
    /// `XFS_ILOG_DEXT` or `XFS_ILOG_DBROOT`.
    pub fields: u32,
    pub format: Format,
    /// Blocks the tree holds, counted in `di_nblocks`.
    pub tree_blocks: u64,
    /// The new tree's blocks.
    pub items: Vec<BufferItem>,
}

/// One block of a tree being built, before it has an address.
struct Built {
    level: u16,
    /// Leaf records, or child keys and the children's indices in the
    /// level below.
    records: Vec<Extent>,
    children: Vec<(u64, usize)>,
}

/// Share `n` items out over as few groups of at most `max` as hold them,
/// as evenly as they go.
pub(crate) fn shares(n: usize, max: usize) -> Vec<usize> {
    let groups = n.div_ceil(max).max(1);
    (0..groups)
        .map(|i| n / groups + usize::from(i < n % groups))
        .collect()
}

impl Filesystem {
    /// Lay `extents` out as the data fork of directory `ino`, giving back
    /// whatever tree it had.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] on a v4 filesystem, whose blocks this
    /// does not build, and whatever reading the old tree or taking blocks
    /// for the new one returns.
    pub(crate) fn map_data_fork<'a>(
        &'a self,
        allocations: &mut Allocations<'a>,
        ino: u64,
        inode: &Inode,
        raw: &[u8],
        extents: &[Extent],
    ) -> Result<ForkMap> {
        let sb = &self.sb;
        let (start, end) = inode.data_fork_range(usize::from(sb.inodesize));
        let fork_len = end - start;

        // What it had goes back first: the new tree is built from nothing.

        if inode.format == Format::Btree {
            let bs = sb.blocksize as usize;
            let (_, blocks) = crate::bmbt::walk_with_blocks(
                &raw[start..end],
                inode.nextents,
                sb,
                ino,
                |fsblock| {
                    let mut buf = vec![0u8; bs];
                    self.device()
                        .read_at(sb.fsblock_offset(fsblock), &mut buf)?;
                    Ok(buf)
                },
            )?;
            for fsblock in blocks {
                let (ag, agbno) = sb.split_fsblock(fsblock);
                let group = allocations.group(sb, self.device(), ag)?;
                group.forget_rmap(crate::rmap::Rmap {
                    startblock: agbno,
                    blockcount: 1,
                    owner: ino as i64,
                    offset: crate::rmap::OFF_BMBT_BLOCK,
                })?;
                group.give_back(crate::alloc_btree::FreeExtent {
                    startblock: agbno,
                    blockcount: 1,
                })?;
            }
        }

        if extents.len() * SLOT <= fork_len {
            let mut fork = Vec::with_capacity(extents.len() * SLOT);
            for e in extents {
                fork.extend_from_slice(&e.to_bytes()?);
            }
            return Ok(ForkMap {
                logged: fork.clone(),
                fork,
                fields: XFS_ILOG_DEXT,
                format: Format::Extents,
                tree_blocks: 0,
                items: Vec::new(),
            });
        }
        if !sb.is_v5() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {ino} needs a block-map B+tree, which this driver builds on v5 only"
            )));
        }

        // Bottom up: leaves, then nodes, until one level fits the root.
        let block_max = (sb.blocksize as usize - HEADER) / SLOT;
        let root_max = (fork_len - ROOT_HEADER) / SLOT;
        let mut built: Vec<Built> = Vec::new();
        let mut level: Vec<usize> = Vec::new();
        let mut at = 0;
        for n in shares(extents.len(), block_max) {
            level.push(built.len());
            built.push(Built {
                level: 0,
                records: extents[at..at + n].to_vec(),
                children: Vec::new(),
            });
            at += n;
        }
        let first_key = |built: &[Built], i: usize| -> u64 {
            match built[i].records.first() {
                Some(e) => e.startoff,
                None => built[i].children[0].0,
            }
        };
        let mut height = 1u16;
        while level.len() > root_max {
            let mut above = Vec::new();
            let mut at = 0;
            for n in shares(level.len(), block_max) {
                let children = level[at..at + n]
                    .iter()
                    .map(|&i| (first_key(&built, i), i))
                    .collect();
                above.push(built.len());
                built.push(Built {
                    level: height,
                    records: Vec::new(),
                    children,
                });
                at += n;
            }
            level = above;
            height += 1;
            if height > 8 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {ino}'s {} extents need a deeper block map than XFS allows",
                    extents.len()
                )));
            }
        }

        // Every block an address, from the inode's own group.
        let (agno, _, _) = sb.split_ino(ino);
        let mut fsblocks = Vec::with_capacity(built.len());
        for _ in &built {
            let agblock = allocations.group(sb, self.device(), agno)?.take(
                1,
                ino as i64,
                crate::rmap::OFF_BMBT_BLOCK,
            )?;
            fsblocks.push((u64::from(agno) << sb.agblklog) | u64::from(agblock));
        }

        // The blocks, each level's threaded left to right.
        let bs = sb.blocksize as usize;
        let mut items = Vec::with_capacity(built.len());
        for (i, b) in built.iter().enumerate() {
            let same: Vec<usize> = (0..built.len())
                .filter(|&j| built[j].level == b.level)
                .collect();
            let pos = same.iter().position(|&j| j == i).expect("itself");
            let left = pos.checked_sub(1).map_or(NULL_BLOCK, |p| fsblocks[same[p]]);
            let right = same.get(pos + 1).map_or(NULL_BLOCK, |&j| fsblocks[j]);
            let mut block = vec![0u8; bs];
            use crate::bmbt::offsets as o;
            block[o::MAGIC..o::MAGIC + 4]
                .copy_from_slice(&crate::bmbt::XFS_BMAP_CRC_MAGIC.to_be_bytes());
            block[o::LEVEL..o::LEVEL + 2].copy_from_slice(&b.level.to_be_bytes());
            let count = b.records.len().max(b.children.len()) as u16;
            block[o::NUMRECS..o::NUMRECS + 2].copy_from_slice(&count.to_be_bytes());
            block[8..16].copy_from_slice(&left.to_be_bytes());
            block[16..24].copy_from_slice(&right.to_be_bytes());
            let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblocks[i]);
            block[o::BLKNO..o::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
            block[o::UUID..o::UUID + 16].copy_from_slice(&sb.meta_uuid);
            block[o::OWNER..o::OWNER + 8].copy_from_slice(&ino.to_be_bytes());
            if b.level == 0 {
                for (k, e) in b.records.iter().enumerate() {
                    block[HEADER + k * SLOT..HEADER + (k + 1) * SLOT]
                        .copy_from_slice(&e.to_bytes()?);
                }
            } else {
                for (k, &(key, child)) in b.children.iter().enumerate() {
                    let key_at = HEADER + k * 8;
                    block[key_at..key_at + 8].copy_from_slice(&key.to_be_bytes());
                    let ptr_at = HEADER + block_max * 8 + k * 8;
                    block[ptr_at..ptr_at + 8].copy_from_slice(&fsblocks[child].to_be_bytes());
                }
            }
            items.push(changed_chunks(blkno, &vec![0u8; bs], block, BLFT_BTREE));
        }

        // The root: stored with its pointers after room for `root_max`
        // keys, logged with them straight after the keys it has.
        let n = level.len();
        let mut fork = vec![0u8; fork_len];
        fork[0..2].copy_from_slice(&height.to_be_bytes());
        fork[2..4].copy_from_slice(&(n as u16).to_be_bytes());
        let mut logged = vec![0u8; HEADER + n * SLOT];
        {
            use crate::bmbt::offsets as o;
            logged[o::MAGIC..o::MAGIC + 4]
                .copy_from_slice(&crate::bmbt::XFS_BMAP_CRC_MAGIC.to_be_bytes());
            logged[o::LEVEL..o::LEVEL + 2].copy_from_slice(&height.to_be_bytes());
            logged[o::NUMRECS..o::NUMRECS + 2].copy_from_slice(&(n as u16).to_be_bytes());
            logged[8..16].copy_from_slice(&NULL_BLOCK.to_be_bytes());
            logged[16..24].copy_from_slice(&NULL_BLOCK.to_be_bytes());
            logged[o::BLKNO..o::BLKNO + 8].copy_from_slice(&NULL_BLOCK.to_be_bytes());
            logged[o::UUID..o::UUID + 16].copy_from_slice(&sb.meta_uuid);
            logged[o::OWNER..o::OWNER + 8].copy_from_slice(&ino.to_be_bytes());
        }
        for (k, &child) in level.iter().enumerate() {
            let key = first_key(&built, child).to_be_bytes();
            let ptr = fsblocks[child].to_be_bytes();
            fork[ROOT_HEADER + k * 8..ROOT_HEADER + k * 8 + 8].copy_from_slice(&key);
            let p = ROOT_HEADER + root_max * 8 + k * 8;
            fork[p..p + 8].copy_from_slice(&ptr);
            logged[HEADER + k * 8..HEADER + k * 8 + 8].copy_from_slice(&key);
            let p = HEADER + n * 8 + k * 8;
            logged[p..p + 8].copy_from_slice(&ptr);
        }
        Ok(ForkMap {
            fork,
            logged,
            fields: XFS_ILOG_DBROOT,
            format: Format::Btree,
            tree_blocks: built.len() as u64,
            items,
        })
    }
}

/// Convert a B+tree root as the log carries it into the shape the inode
/// stores, for a data fork of `fork_len` bytes (`xfs_bmbt_to_bmdr`).
///
/// # Errors
///
/// [`Error::CorruptLog`] when the logged root is shorter than its own
/// header or claims more entries than either shape holds.
pub(crate) fn logged_root_to_disk(logged: &[u8], v5: bool, fork_len: usize) -> Result<Vec<u8>> {
    let header = if v5 { HEADER } else { 24 };
    let bad = |why: String| Error::CorruptLog(format!("a logged block-map root {why}"));
    if logged.len() < header || fork_len < ROOT_HEADER {
        return Err(bad(format!(
            "is {} bytes, shorter than its header",
            logged.len()
        )));
    }
    let level = &logged[4..6];
    let n = usize::from(u16::from_be_bytes([logged[6], logged[7]]));
    let logged_max = (logged.len() - header) / SLOT;
    let disk_max = (fork_len - ROOT_HEADER) / SLOT;
    if n > logged_max || n > disk_max {
        return Err(bad(format!("claims {n} entries")));
    }
    let mut out = vec![0u8; fork_len];
    out[0..2].copy_from_slice(level);
    out[2..4].copy_from_slice(&logged[6..8]);
    for k in 0..n {
        out[ROOT_HEADER + k * 8..ROOT_HEADER + k * 8 + 8]
            .copy_from_slice(&logged[header + k * 8..header + k * 8 + 8]);
        let from = header + logged_max * 8 + k * 8;
        let to = ROOT_HEADER + disk_max * 8 + k * 8;
        out[to..to + 8].copy_from_slice(&logged[from..from + 8]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_are_even_and_hold_everything() {
        assert_eq!(shares(21, 59), vec![21]);
        assert_eq!(shares(60, 59), vec![30, 30]);
        assert_eq!(shares(119, 59), vec![40, 40, 39]);
        for n in 1..1000 {
            let s = shares(n, 59);
            assert_eq!(s.iter().sum::<usize>(), n);
            assert!(s.iter().all(|&k| k <= 59));
            if s.len() > 1 {
                assert!(s.iter().all(|&k| k >= 59 / 2), "{n}: {s:?}");
            }
        }
    }

    #[test]
    fn a_logged_root_lands_where_the_reader_looks() {
        let n = 3;
        let mut logged = vec![0u8; HEADER + n * SLOT];
        logged[4..6].copy_from_slice(&1u16.to_be_bytes());
        logged[6..8].copy_from_slice(&(n as u16).to_be_bytes());
        for k in 0..n {
            logged[HEADER + k * 8..HEADER + k * 8 + 8]
                .copy_from_slice(&(k as u64 * 100).to_be_bytes());
            let p = HEADER + n * 8 + k * 8;
            logged[p..p + 8].copy_from_slice(&(5000 + k as u64).to_be_bytes());
        }
        let fork_len = 336;
        let disk = logged_root_to_disk(&logged, true, fork_len).unwrap();
        let max = (fork_len - ROOT_HEADER) / SLOT;
        for k in 0..n {
            let key = u64::from_be_bytes(disk[4 + k * 8..12 + k * 8].try_into().unwrap());
            let p = ROOT_HEADER + max * 8 + k * 8;
            let ptr = u64::from_be_bytes(disk[p..p + 8].try_into().unwrap());
            assert_eq!((key, ptr), (k as u64 * 100, 5000 + k as u64));
        }
    }
}
