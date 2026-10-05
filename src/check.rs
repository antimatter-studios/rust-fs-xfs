//! Checking a filesystem without changing it (#339).
//!
//! What `fsck.xfs` runs: a walk of every structure the volume declares,
//! with each one checked against the others that describe the same thing.
//! Nothing is written, and nothing is repaired. A volume this calls clean
//! is one on which none of the invariants below is broken; it is a subset
//! of what `xfs_repair -n` checks, and says so.
//!
//! # What is checked
//!
//! - **The superblock copies**: every secondary agrees with the primary on
//!   the geometry and the features.
//! - **Each allocation group's headers against its trees**: the two
//!   free-space btrees hold the same extents, in order and without
//!   overlap, and the AGF's free count, longest extent and btree block
//!   count are what they add up to; the inode btree's chunks are
//!   consistent with themselves and the AGI's counts, and the free inode
//!   btree holds exactly the chunks with a free inode.
//! - **Every block has one owner.** Headers, btree blocks, the free list,
//!   the log, inode chunks, free space and every inode's data, attribute
//!   and extent-tree blocks are each claimed once. A block claimed twice
//!   is a cross-link (unless the refcount btree says it is shared between
//!   files); a block claimed by nothing is lost.
//! - **Every inode the inode btree calls allocated is one**, and every
//!   inode it calls free is free, each with a valid checksum.
//! - **The directory tree**: walked from the root, every entry points at
//!   an allocated inode of the type the entry records, every allocated
//!   inode is reached, and every link count is the number of entries
//!   that reach it.
//! - **The superblock's counters** against the groups' totals, on a
//!   volume whose log was clean. A volume that needed replay is reported
//!   as dirty, and is checked as the replay leaves it.
//!
//! The walk reads through the ordinary read-only mount, so the volume is
//! checked as the kernel would see it after recovering its log, and the
//! device is not touched.

use crate::ag_btree;
use crate::alloc_btree::{FreeExtent, Order};
use crate::error::Result;
use crate::fs::Filesystem;
use crate::inode::{FileType, Format};
use crate::inode_btree::{InodeChunk, Which, INODES_PER_CHUNK};
use std::collections::{BTreeMap, HashMap, HashSet};

/// One thing found wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// The allocation group it was found in, where it belongs to one.
    pub ag: Option<u32>,
    /// The inode it concerns, where it concerns one.
    pub ino: Option<u64>,
    /// What is wrong, in words.
    pub what: String,
}

/// What a check found.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Everything found wrong, in the order it was found.
    pub findings: Vec<Finding>,
    /// The log held records that had not been applied. The volume was
    /// checked as replaying them leaves it.
    pub dirty: bool,
    /// Allocated inodes walked.
    pub inodes: u64,
    /// Directories walked.
    pub directories: u64,
    /// Free blocks counted across every group.
    pub free_blocks: u64,
}

impl Report {
    /// True when nothing was found wrong.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty()
    }
}

/// The most findings reported of one kind in one group, so a wholesale
/// corruption does not print a million lines.
const PER_KIND: usize = 20;

/// Who a block belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    Unclaimed,
    Headers,
    Btree(&'static str),
    FreeList,
    Log,
    Inodes,
    Free,
    Data(u64),
    Attr(u64),
    ExtentTree(u64),
}

impl Owner {
    fn describe(self) -> String {
        match self {
            Owner::Unclaimed => "nothing".into(),
            Owner::Headers => "the group headers".into(),
            Owner::Btree(name) => format!("the {name} btree"),
            Owner::FreeList => "the free list".into(),
            Owner::Log => "the log".into(),
            Owner::Inodes => "an inode chunk".into(),
            Owner::Free => "free space".into(),
            Owner::Data(ino) => format!("inode {ino}'s data"),
            Owner::Attr(ino) => format!("inode {ino}'s attributes"),
            Owner::ExtentTree(ino) => format!("inode {ino}'s extent tree"),
        }
    }
    fn is_file_data(self) -> bool {
        matches!(self, Owner::Data(_))
    }
}

struct Checker<'a> {
    fs: &'a Filesystem,
    report: Report,
    /// Per group, the owner of every block.
    owners: Vec<Vec<Owner>>,
    /// Per group, the blocks the refcount btree says files share.
    shared: Vec<HashSet<u32>>,
    /// Per (group, kind), how many findings were reported.
    counted: HashMap<(Option<u32>, &'static str), usize>,
    /// Every inode the inode btrees call allocated, and every one they
    /// call free.
    allocated: HashSet<u64>,
    free: HashSet<u64>,
}

/// Check `fs`. Never writes.
pub fn check(fs: &Filesystem) -> Report {
    let sb = fs.superblock();
    let owners = (0..sb.agcount)
        .map(|ag| vec![Owner::Unclaimed; ag_length(fs, ag) as usize])
        .collect();
    let mut c = Checker {
        fs,
        report: Report {
            dirty: fs.was_replayed(),
            ..Report::default()
        },
        owners,
        shared: vec![HashSet::new(); sb.agcount as usize],
        counted: HashMap::new(),
        allocated: HashSet::new(),
        free: HashSet::new(),
    };
    c.secondaries();
    let mut totals = Totals::default();
    for ag in 0..sb.agcount {
        c.group(ag, &mut totals);
    }
    c.inodes();
    c.unclaimed();
    c.counters(&totals);
    c.report
}

/// The length of group `ag`: every group is `agblocks` but the last.
fn ag_length(fs: &Filesystem, ag: u32) -> u32 {
    let sb = fs.superblock();
    let before = u64::from(ag) * u64::from(sb.agblocks);
    (sb.dblocks - before).min(u64::from(sb.agblocks)) as u32
}

#[derive(Default)]
struct Totals {
    icount: u64,
    ifree: u64,
    fdblocks: u64,
}

impl Checker<'_> {
    fn find(&mut self, kind: &'static str, ag: Option<u32>, ino: Option<u64>, what: String) {
        let n = self.counted.entry((ag, kind)).or_insert(0);
        *n += 1;
        if *n <= PER_KIND {
            self.report.findings.push(Finding { ag, ino, what });
        } else if *n == PER_KIND + 1 {
            self.report.findings.push(Finding {
                ag,
                ino: None,
                what: format!("more findings of this kind ({kind}) are not listed"),
            });
        }
    }

    /// Claim `len` blocks at `start` in group `ag` for `owner`.
    fn claim(&mut self, ag: u32, start: u32, len: u32, owner: Owner) {
        let Some(map) = self.owners.get_mut(ag as usize) else {
            self.find(
                "range",
                None,
                None,
                format!(
                    "{} claims blocks in group {ag}, which does not exist",
                    owner.describe()
                ),
            );
            return;
        };
        let length = map.len() as u32;
        if start >= length || len > length - start {
            let what = format!(
                "{} claims blocks {start}..{} of group {ag}, which has {length}",
                owner.describe(),
                u64::from(start) + u64::from(len)
            );
            self.find("range", Some(ag), owner_ino(owner), what);
            return;
        }
        let mut clash: Option<(u32, Owner)> = None;
        for b in start..start + len {
            let was = map[b as usize];
            if was == Owner::Unclaimed {
                map[b as usize] = owner;
            } else if was.is_file_data()
                && owner.is_file_data()
                && self.shared[ag as usize].contains(&b)
            {
                // Shared between files, as the refcount btree says.
            } else if clash.is_none() {
                clash = Some((b, was));
            }
        }
        if let Some((b, was)) = clash {
            let what = format!(
                "block {b} of group {ag} belongs to {} and to {}",
                was.describe(),
                owner.describe()
            );
            self.find("cross-link", Some(ag), owner_ino(owner), what);
        }
    }

    fn claim_fsblocks(&mut self, fsblock: u64, len: u64, owner: Owner) {
        let sb = self.fs.superblock();
        let (ag, agbno) = sb.split_fsblock(fsblock);
        match u32::try_from(len) {
            Ok(len) => self.claim(ag, agbno, len, owner),
            Err(_) => self.find(
                "range",
                Some(ag),
                owner_ino(owner),
                format!("{} claims {len} blocks in one extent", owner.describe()),
            ),
        }
    }

    /// Every secondary superblock agrees with the primary on what the
    /// filesystem is.
    fn secondaries(&mut self) {
        let sb = self.fs.superblock().clone();
        for ag in 1..sb.agcount {
            let mut raw = vec![0u8; usize::from(sb.sectsize)];
            let at = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize);
            if let Err(e) = self.fs.device().read_at(at, &mut raw) {
                self.find(
                    "superblock",
                    Some(ag),
                    None,
                    format!("reading the superblock copy: {e}"),
                );
                continue;
            }
            let copy = match crate::superblock::Superblock::parse_copy(&raw) {
                Ok(copy) => copy,
                Err(e) => {
                    self.find(
                        "superblock",
                        Some(ag),
                        None,
                        format!("the superblock copy: {e}"),
                    );
                    continue;
                }
            };
            let fields: [(&str, u64, u64); 12] = [
                ("blocksize", sb.blocksize.into(), copy.blocksize.into()),
                ("dblocks", sb.dblocks, copy.dblocks),
                ("agblocks", sb.agblocks.into(), copy.agblocks.into()),
                ("agcount", sb.agcount.into(), copy.agcount.into()),
                ("logstart", sb.logstart, copy.logstart),
                ("logblocks", sb.logblocks.into(), copy.logblocks.into()),
                // The kernel turns the attribute and quota bits on in the
                // primary the first time it needs them, and leaves the
                // copies as they were: a difference there is history,
                // not damage.
                (
                    "versionnum",
                    (sb.versionnum & !LATE_VERSION_BITS).into(),
                    (copy.versionnum & !LATE_VERSION_BITS).into(),
                ),
                ("sectsize", sb.sectsize.into(), copy.sectsize.into()),
                ("inodesize", sb.inodesize.into(), copy.inodesize.into()),
                (
                    "features_compat",
                    sb.features_compat.into(),
                    copy.features_compat.into(),
                ),
                (
                    "features_ro_compat",
                    sb.features_ro_compat.into(),
                    copy.features_ro_compat.into(),
                ),
                (
                    "features_incompat",
                    sb.features_incompat.into(),
                    copy.features_incompat.into(),
                ),
            ];
            for (name, primary, secondary) in fields {
                if primary != secondary {
                    self.find(
                        "superblock",
                        Some(ag),
                        None,
                        format!("the superblock copy says {name} {secondary}; the primary says {primary}"),
                    );
                }
            }
            if copy.uuid != sb.uuid {
                self.find(
                    "superblock",
                    Some(ag),
                    None,
                    "the superblock copy has another filesystem's UUID".into(),
                );
            }
        }
    }

    /// One group: its headers, its trees, and the blocks they own.
    fn group(&mut self, ag: u32, totals: &mut Totals) {
        let fs = self.fs;
        let sb = fs.superblock();
        let header_blocks = (4 * u32::from(sb.sectsize)).div_ceil(sb.blocksize);
        self.claim(ag, 0, header_blocks, Owner::Headers);

        if sb.has_internal_log() {
            let (log_ag, log_bno) = sb.split_fsblock(sb.logstart);
            if log_ag == ag {
                self.claim(ag, log_bno, sb.logblocks, Owner::Log);
            }
        }

        let agf = match fs.read_agf(ag) {
            Ok(agf) => agf,
            Err(e) => {
                self.find("header", Some(ag), None, format!("the AGF: {e}"));
                return;
            }
        };
        let agi = match fs.read_agi(ag) {
            Ok(agi) => agi,
            Err(e) => {
                self.find("header", Some(ag), None, format!("the AGI: {e}"));
                return;
            }
        };
        if agf.length != ag_length(fs, ag) || agi.length != agf.length {
            self.find(
                "header",
                Some(ag),
                None,
                format!(
                    "the AGF says the group is {} blocks and the AGI {}; the geometry says {}",
                    agf.length,
                    agi.length,
                    ag_length(fs, ag)
                ),
            );
        }

        let ag_start = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize);
        let read = |agblock: u32| -> Result<Vec<u8>> {
            let mut raw = vec![0u8; sb.blocksize as usize];
            fs.device().read_at(
                ag_start + u64::from(agblock) * u64::from(sb.blocksize),
                &mut raw,
            )?;
            Ok(raw)
        };

        // The free list.
        let mut agfl_raw = vec![0u8; usize::from(sb.sectsize)];
        let agfl = fs
            .device()
            .read_at(ag_start + 3 * u64::from(sb.sectsize), &mut agfl_raw)
            .map_err(crate::error::Error::from)
            .and_then(|_| crate::agfl::Agfl::parse(&agfl_raw, sb, &agf, ag));
        match agfl {
            Ok(agfl) => {
                for b in agfl.entries(sb) {
                    self.claim(ag, b, 1, Owner::FreeList);
                }
            }
            Err(e) => self.find("header", Some(ag), None, format!("the free list: {e}")),
        }

        // The refcount btree first: which blocks may be claimed twice.
        let mut refcount_blocks = 0u32;
        if sb.has_reflink() && agf.refcount_level > 0 {
            match ag_btree::walk_blocks(
                sb,
                crate::refcount::shape(),
                ag,
                agf.refcount_root,
                agf.refcount_level,
                read,
                crate::refcount::decode,
            ) {
                Ok((records, blocks)) => {
                    refcount_blocks = blocks.len() as u32;
                    for b in blocks {
                        self.claim(ag, b, 1, Owner::Btree("refcount"));
                    }
                    for r in records {
                        if r.refcount > 1 && !r.cow {
                            for b in r.startblock..r.startblock.saturating_add(r.blockcount) {
                                self.shared[ag as usize].insert(b);
                            }
                        }
                    }
                }
                Err(e) => self.find("btree", Some(ag), None, format!("the refcount btree: {e}")),
            }
        }
        let _ = refcount_blocks;

        // The free-space btrees.
        let mut btree_blocks = 0u32;
        let mut by_block: Vec<FreeExtent> = Vec::new();
        let mut by_count: Vec<FreeExtent> = Vec::new();
        for (order, name, out) in [
            (Order::ByBlock, "free space by block", &mut by_block),
            (Order::ByCount, "free space by count", &mut by_count),
        ] {
            let which = match order {
                Order::ByBlock => crate::ag::agf_btree::BNO,
                Order::ByCount => crate::ag::agf_btree::CNT,
            };
            match ag_btree::walk_blocks(
                sb,
                order.shape(),
                ag,
                agf.roots[which],
                agf.levels[which],
                read,
                crate::alloc_btree::decode_free_extent,
            ) {
                Ok((records, blocks)) => {
                    btree_blocks += blocks.len() as u32 - 1;
                    for b in blocks {
                        self.claim(ag, b, 1, Owner::Btree(name));
                    }
                    *out = records;
                }
                Err(e) => self.find("btree", Some(ag), None, format!("the {name} btree: {e}")),
            }
        }
        if sb.has_rmapbt() && agf.levels[crate::ag::agf_btree::RMAP] > 0 {
            let which = crate::ag::agf_btree::RMAP;
            match ag_btree::walk_blocks(
                sb,
                crate::rmap::shape(),
                ag,
                agf.roots[which],
                agf.levels[which],
                read,
                |_, _| (),
            ) {
                Ok((_, blocks)) => {
                    btree_blocks += blocks.len() as u32 - 1;
                    for b in blocks {
                        self.claim(ag, b, 1, Owner::Btree("reverse mapping"));
                    }
                }
                Err(e) => self.find(
                    "btree",
                    Some(ag),
                    None,
                    format!("the reverse-mapping btree: {e}"),
                ),
            }
        }
        self.free_space(ag, &agf, &by_block, &by_count, btree_blocks);
        let free: u32 = by_block.iter().map(|e| e.blockcount).sum();
        totals.fdblocks += u64::from(free) + u64::from(agf.flcount) + u64::from(agf.btreeblks);
        self.report.free_blocks += u64::from(free);

        // The inode btrees, and the AGI's count of their blocks.
        let agi_blocks = {
            let mut raw = vec![0u8; usize::from(sb.sectsize)];
            match fs
                .device()
                .read_at(ag_start + 2 * u64::from(sb.sectsize), &mut raw)
            {
                Ok(()) => (be32(&raw, AGI_IBLOCKS), be32(&raw, AGI_IBLOCKS + 4)),
                Err(_) => (0, 0),
            }
        };
        let sparse = sb.has_sparse_inodes();
        let chunks = match ag_btree::walk_blocks(
            sb,
            crate::inode_btree::shape(Which::All, sb.is_v5()),
            ag,
            agi.root,
            agi.level,
            read,
            |buf, at| crate::inode_btree::record(buf, at, sparse),
        ) {
            Ok((records, blocks)) => {
                if has_inobt_counts(sb) && blocks.len() as u32 != agi_blocks.0 {
                    self.find(
                        "counter",
                        Some(ag),
                        None,
                        format!(
                            "the AGI says the inode btree is {} blocks; it is {}",
                            agi_blocks.0,
                            blocks.len()
                        ),
                    );
                }
                for b in blocks {
                    self.claim(ag, b, 1, Owner::Btree("inode"));
                }
                let mut chunks = Vec::new();
                for r in records {
                    match r {
                        Ok(chunk) => chunks.push(chunk),
                        Err(e) => self.find(
                            "btree",
                            Some(ag),
                            None,
                            format!("an inode btree record: {e}"),
                        ),
                    }
                }
                chunks
            }
            Err(e) => {
                self.find("btree", Some(ag), None, format!("the inode btree: {e}"));
                Vec::new()
            }
        };
        if sb.has_finobt() && agi.free_level > 0 {
            match ag_btree::walk_blocks(
                sb,
                crate::inode_btree::shape(Which::WithFreeInodes, sb.is_v5()),
                ag,
                agi.free_root,
                agi.free_level,
                read,
                |buf, at| crate::inode_btree::record(buf, at, sparse),
            ) {
                Ok((records, blocks)) => {
                    if has_inobt_counts(sb) && blocks.len() as u32 != agi_blocks.1 {
                        self.find(
                            "counter",
                            Some(ag),
                            None,
                            format!(
                                "the AGI says the free inode btree is {} blocks; it is {}",
                                agi_blocks.1,
                                blocks.len()
                            ),
                        );
                    }
                    for b in blocks {
                        self.claim(ag, b, 1, Owner::Btree("free inode"));
                    }
                    let got: Vec<InodeChunk> = records.into_iter().filter_map(|r| r.ok()).collect();
                    let want: Vec<InodeChunk> =
                        chunks.iter().copied().filter(|c| c.freecount > 0).collect();
                    if got != want {
                        self.find(
                            "btree",
                            Some(ag),
                            None,
                            format!(
                                "the free inode btree holds {} chunks; the inode btree has {} with a \
                                 free inode, and they are not the same",
                                got.len(),
                                want.len()
                            ),
                        );
                    }
                }
                Err(e) => self.find(
                    "btree",
                    Some(ag),
                    None,
                    format!("the free inode btree: {e}"),
                ),
            }
        }
        self.chunks(ag, &agi, &chunks, totals);
    }

    /// The free-space btrees against each other and the AGF.
    fn free_space(
        &mut self,
        ag: u32,
        agf: &crate::ag::Agf,
        by_block: &[FreeExtent],
        by_count: &[FreeExtent],
        btree_blocks: u32,
    ) {
        let mut end = 0u64;
        for (i, e) in by_block.iter().enumerate() {
            if e.blockcount == 0 {
                self.find(
                    "free space",
                    Some(ag),
                    None,
                    format!("free extent {i} is empty"),
                );
            }
            if i > 0 && u64::from(e.startblock) < end {
                self.find(
                    "free space",
                    Some(ag),
                    None,
                    format!(
                        "free extent at block {} overlaps or precedes the one before it",
                        e.startblock
                    ),
                );
            }
            end = u64::from(e.startblock) + u64::from(e.blockcount);
            self.claim(ag, e.startblock, e.blockcount, Owner::Free);
        }
        for w in by_count.windows(2) {
            if (w[0].blockcount, w[0].startblock) >= (w[1].blockcount, w[1].startblock) {
                self.find(
                    "free space",
                    Some(ag),
                    None,
                    format!(
                        "the free-space-by-count btree is out of order at the extent at block {}",
                        w[1].startblock
                    ),
                );
                break;
            }
        }
        let mut a: Vec<(u32, u32)> = by_block
            .iter()
            .map(|e| (e.startblock, e.blockcount))
            .collect();
        let mut b: Vec<(u32, u32)> = by_count
            .iter()
            .map(|e| (e.startblock, e.blockcount))
            .collect();
        a.sort_unstable();
        b.sort_unstable();
        if a != b {
            self.find(
                "free space",
                Some(ag),
                None,
                format!(
                    "the two free-space btrees disagree: {} extents by block, {} by count, not the same",
                    a.len(),
                    b.len()
                ),
            );
        }
        let free: u64 = by_block.iter().map(|e| u64::from(e.blockcount)).sum();
        if free != u64::from(agf.freeblks) {
            self.find(
                "counter",
                Some(ag),
                None,
                format!(
                    "the AGF counts {} free blocks; the free-space btree holds {free}",
                    agf.freeblks
                ),
            );
        }
        let longest = by_block.iter().map(|e| e.blockcount).max().unwrap_or(0);
        if longest != agf.longest {
            self.find(
                "counter",
                Some(ag),
                None,
                format!(
                    "the AGF says the longest free extent is {}; it is {longest}",
                    agf.longest
                ),
            );
        }
        let sb = self.fs.superblock();
        if has_lazy_counters(sb) && agf.btreeblks != btree_blocks {
            self.find(
                "counter",
                Some(ag),
                None,
                format!(
                    "the AGF counts {} free-space btree blocks past the roots; there are {btree_blocks}",
                    agf.btreeblks
                ),
            );
        }
    }

    /// The chunks against themselves and the AGI, and the inodes they
    /// hold against what the chunks say about them.
    fn chunks(
        &mut self,
        ag: u32,
        agi: &crate::ag::Agi,
        chunks: &[InodeChunk],
        totals: &mut Totals,
    ) {
        let sb = self.fs.superblock().clone();
        let mut count = 0u64;
        let mut freecount = 0u64;
        for chunk in chunks {
            let existing = (0..INODES_PER_CHUNK).filter(|&n| chunk.exists(n)).count() as u64;
            let free = (0..INODES_PER_CHUNK)
                .filter(|&n| chunk.exists(n) && chunk.is_free(n))
                .count() as u64;
            if existing != u64::from(chunk.count) || free != u64::from(chunk.freecount) {
                self.find(
                    "inode btree",
                    Some(ag),
                    None,
                    format!(
                        "the chunk at inode {} says {} inodes, {} free; its masks say {existing}, {free}",
                        chunk.startino, chunk.count, chunk.freecount
                    ),
                );
            }
            count += u64::from(chunk.count);
            freecount += u64::from(chunk.freecount);
            let mut blocks_seen = HashSet::new();
            for n in 0..INODES_PER_CHUNK {
                if !chunk.exists(n) {
                    continue;
                }
                let agino = chunk.startino + u32::from(n);
                let agbno = agino >> sb.inopblog;
                if blocks_seen.insert(agbno) {
                    self.claim(ag, agbno, 1, Owner::Inodes);
                }
                let ino = sb.join_ino(ag, agino);
                if chunk.is_free(n) {
                    self.free.insert(ino);
                } else {
                    self.allocated.insert(ino);
                }
            }
        }
        if count != u64::from(agi.count) || freecount != u64::from(agi.freecount) {
            self.find(
                "counter",
                Some(ag),
                None,
                format!(
                    "the AGI counts {} inodes, {} free; the inode btree holds {count}, {freecount}",
                    agi.count, agi.freecount
                ),
            );
        }
        totals.icount += count;
        totals.ifree += freecount;
    }

    /// Every inode: allocated ones are in use and checksum, free ones are
    /// free; then the directory tree from the root.
    fn inodes(&mut self) {
        let fs = self.fs;
        let sb = fs.superblock().clone();
        let mut free: Vec<u64> = self.free.iter().copied().collect();
        free.sort_unstable();
        for ino in free {
            let (ag, _, _) = sb.split_ino(ino);
            match fs.read_inode_raw(ino) {
                Ok((inode, _)) if inode.mode != 0 => self.find(
                    "inode",
                    Some(ag),
                    Some(ino),
                    format!(
                        "inode {ino} is free in the inode btree but in use (mode {:o})",
                        inode.mode
                    ),
                ),
                Ok(_) => {}
                // A free inode that has never been used may never have
                // been written; only an allocated one must parse.
                Err(_) => {}
            }
        }

        let metadata: HashSet<u64> = [sb.rbmino, sb.rsumino, sb.uquotino, sb.gquotino, sb.pquotino]
            .into_iter()
            .filter(|&i| i != 0 && i != u64::MAX)
            .collect();

        let mut allocated: Vec<u64> = self.allocated.iter().copied().collect();
        allocated.sort_unstable();
        let mut usable: BTreeMap<u64, crate::inode::Inode> = BTreeMap::new();
        let mut raws: HashMap<u64, Vec<u8>> = HashMap::new();
        for ino in allocated {
            let (ag, _, _) = sb.split_ino(ino);
            match fs.read_inode_raw(ino) {
                Ok((inode, raw)) => {
                    if inode.mode == 0 {
                        self.find(
                            "inode",
                            Some(ag),
                            Some(ino),
                            format!("inode {ino} is allocated in the inode btree but not in use"),
                        );
                        continue;
                    }
                    self.report.inodes += 1;
                    self.blocks_of(&inode, &raw);
                    raws.insert(ino, raw);
                    usable.insert(ino, inode);
                }
                Err(e) => self.find("inode", Some(ag), Some(ino), format!("inode {ino}: {e}")),
            }
        }

        // The tree, from the root.
        let mut links: HashMap<u64, u32> = HashMap::new();
        let mut subdirs: HashMap<u64, u32> = HashMap::new();
        let mut reached: HashSet<u64> = HashSet::new();
        let mut stack = vec![sb.rootino];
        reached.insert(sb.rootino);
        if !usable.contains_key(&sb.rootino) {
            let why = if self.allocated.contains(&sb.rootino) {
                "could not be read"
            } else {
                "is not an allocated inode"
            };
            self.find(
                "directory",
                None,
                Some(sb.rootino),
                format!("the root directory, inode {}, {why}", sb.rootino),
            );
            stack.clear();
        }
        while let Some(dir) = stack.pop() {
            let Some(inode) = usable.get(&dir).cloned() else {
                continue;
            };
            if !inode.is_dir() {
                self.find(
                    "directory",
                    None,
                    Some(dir),
                    format!("inode {dir} is reached as a directory and is not one"),
                );
                continue;
            }
            self.report.directories += 1;
            let entries = match fs.read_dir(&inode, &raws[&dir]) {
                Ok(e) => e,
                Err(e) => {
                    self.find(
                        "directory",
                        None,
                        Some(dir),
                        format!("directory {dir}: {e}"),
                    );
                    continue;
                }
            };
            for entry in entries {
                let name = String::from_utf8_lossy(&entry.name).into_owned();
                let Some(target) = usable.get(&entry.ino) else {
                    let why = if self.free.contains(&entry.ino) {
                        "a free inode"
                    } else if self.allocated.contains(&entry.ino) {
                        "an inode that could not be read"
                    } else {
                        "an inode in no chunk"
                    };
                    self.find(
                        "directory",
                        None,
                        Some(dir),
                        format!(
                            "directory {dir}: entry {name:?} points at inode {}, {why}",
                            entry.ino
                        ),
                    );
                    continue;
                };
                *links.entry(entry.ino).or_insert(0) += 1;
                if sb.has_ftype() && entry.ftype.is_some() && entry.ftype != target.file_type() {
                    self.find(
                        "directory",
                        None,
                        Some(dir),
                        format!(
                            "directory {dir}: entry {name:?} says {:?}; inode {} is {:?}",
                            entry.ftype,
                            entry.ino,
                            target.file_type()
                        ),
                    );
                }
                if target.file_type() == Some(FileType::Directory) {
                    *subdirs.entry(dir).or_insert(0) += 1;
                    if !reached.insert(entry.ino) {
                        self.find(
                            "directory",
                            None,
                            Some(entry.ino),
                            format!(
                                "directory {} is reached from more than one place",
                                entry.ino
                            ),
                        );
                        continue;
                    }
                    stack.push(entry.ino);
                } else {
                    reached.insert(entry.ino);
                }
            }
        }

        for (&ino, inode) in &usable {
            if metadata.contains(&ino) {
                continue;
            }
            let (ag, _, _) = sb.split_ino(ino);
            if !reached.contains(&ino) {
                self.find(
                    "directory",
                    Some(ag),
                    Some(ino),
                    format!("inode {ino} is allocated and no directory reaches it"),
                );
                continue;
            }
            let want = if inode.is_dir() {
                2 + subdirs.get(&ino).copied().unwrap_or(0)
            } else {
                links.get(&ino).copied().unwrap_or(0)
            };
            if inode.nlink != want {
                self.find(
                    "link count",
                    Some(ag),
                    Some(ino),
                    format!(
                        "inode {ino} has a link count of {}; {want} entries reach it",
                        inode.nlink
                    ),
                );
            }
        }
    }

    /// Claim every block `inode` owns: its data, its attributes, and the
    /// extent trees above either.
    fn blocks_of(&mut self, inode: &crate::inode::Inode, raw: &[u8]) {
        let sb = self.fs.superblock().clone();
        let ino = inode.ino;
        let isz = usize::from(sb.inodesize);
        if !inode.is_realtime() {
            let (start, end) = inode.data_fork_range(isz);
            self.fork(
                ino,
                inode.format,
                &raw[start..end],
                inode.nextents,
                Owner::Data(ino),
            );
        }
        if let Some((start, end)) = inode.attr_fork_range(isz) {
            self.fork(
                ino,
                inode.aformat,
                &raw[start..end],
                u64::from(inode.anextents),
                Owner::Attr(ino),
            );
        }
    }

    fn fork(&mut self, ino: u64, format: Format, fork: &[u8], nextents: u64, owner: Owner) {
        let sb = self.fs.superblock().clone();
        let (ag, _, _) = sb.split_ino(ino);
        let walked = match format {
            Format::Extents => crate::extent::parse_list(fork, nextents).map(|e| (e, Vec::new())),
            Format::Btree => {
                crate::bmbt::walk_with_blocks(fork, nextents, &sb, ino, |b| self.fs.read_fsblock(b))
            }
            _ => return,
        };
        match walked {
            Ok((extents, tree)) => {
                for b in tree {
                    self.claim_fsblocks(b, 1, Owner::ExtentTree(ino));
                }
                for e in extents {
                    if !sb.extent_in_bounds(e.startblock, e.blockcount) {
                        self.find(
                            "extent",
                            Some(ag),
                            Some(ino),
                            format!(
                                "inode {ino} maps {} blocks at block {}, outside one allocation group",
                                e.blockcount, e.startblock
                            ),
                        );
                        continue;
                    }
                    self.claim_fsblocks(e.startblock, e.blockcount, owner);
                }
            }
            Err(e) => self.find(
                "extent",
                Some(ag),
                Some(ino),
                format!("inode {ino}'s extents: {e}"),
            ),
        }
    }

    /// A block nothing claimed is lost.
    fn unclaimed(&mut self) {
        for ag in 0..self.owners.len() {
            let lost: Vec<u32> = self.owners[ag]
                .iter()
                .enumerate()
                .filter(|(_, o)| **o == Owner::Unclaimed)
                .map(|(b, _)| b as u32)
                .collect();
            if let Some(&first) = lost.first() {
                self.find(
                    "lost",
                    Some(ag as u32),
                    None,
                    format!(
                        "{} blocks of group {ag} belong to nothing, the first at block {first}",
                        lost.len()
                    ),
                );
            }
        }
    }

    /// The superblock's counters against what the groups add up to.
    fn counters(&mut self, totals: &Totals) {
        // A volume that needed replay has counters the kernel recomputes
        // at mount: they say nothing until then.
        if self.report.dirty {
            return;
        }
        let sb = self.fs.superblock().clone();
        for (name, said, counted) in [
            ("icount", sb.icount, totals.icount),
            ("ifree", sb.ifree, totals.ifree),
            ("fdblocks", sb.fdblocks, totals.fdblocks),
        ] {
            if said != counted {
                self.find(
                    "counter",
                    None,
                    None,
                    format!("the superblock says {name} {said}; the groups add up to {counted}"),
                );
            }
        }
    }
}

fn owner_ino(owner: Owner) -> Option<u64> {
    match owner {
        Owner::Data(i) | Owner::Attr(i) | Owner::ExtentTree(i) => Some(i),
        _ => None,
    }
}

/// `agi_iblocks`; `agi_fblocks` follows it.
const AGI_IBLOCKS: usize = crate::ag::offsets::agi::FREE_LEVEL + 4;

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn has_inobt_counts(sb: &crate::superblock::Superblock) -> bool {
    sb.features_ro_compat & crate::superblock::ro_compat::INOBTCNT != 0
}

fn has_lazy_counters(sb: &crate::superblock::Superblock) -> bool {
    sb.features2 & crate::superblock::features2_flags::LAZYSBCOUNT != 0
}

/// `sb_versionnum` bits the kernel sets in the primary superblock alone,
/// on first use: attributes and quotas.
const LATE_VERSION_BITS: u16 =
    crate::superblock::version_flags::ATTRBIT | crate::superblock::version_flags::QUOTABIT;
