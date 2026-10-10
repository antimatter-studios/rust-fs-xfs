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

mod quotas;

/// What a finding is, by a name that does not change (#363).
///
/// The words in [`Finding::what`] are for a person and may be reworded
/// in any release; the code is for a script, and is part of the output
/// schema documented in `docs/fsck-output.md`. A code is never renamed
/// or reused for something else: one that stops being emitted is
/// retired, and a new kind of finding gets a new code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Code {
    /// A metadata block or inode failed its CRC.
    Checksum,
    /// A metadata block's self-describing header names another place.
    Identity,
    /// A secondary superblock could not be read or parsed.
    SbCopyUnreadable,
    /// A secondary superblock disagrees with the primary on a field.
    SbCopyField,
    /// A secondary superblock carries another filesystem's UUID.
    SbCopyUuid,
    /// A group's AGF could not be read.
    AgfUnreadable,
    /// A group's AGI could not be read.
    AgiUnreadable,
    /// A group's free list could not be read.
    AgflUnreadable,
    /// The AGF, the AGI and the geometry disagree on a group's length.
    AgLength,
    /// A btree could not be walked.
    BtreeUnreadable,
    /// An inode btree record could not be decoded.
    InobtRecord,
    /// An inode chunk's counts disagree with its masks.
    InobtChunkCount,
    /// The free inode btree is not the inode btree's chunks with a free inode.
    FinobtMismatch,
    /// A free-space record is empty.
    FreespEmpty,
    /// A free-space record overlaps or precedes the one before it.
    FreespOverlap,
    /// The free-space-by-count btree is out of order.
    FreespCntOrder,
    /// The two free-space btrees hold different extents.
    FreespDisagree,
    /// The AGF's free block count is not what the free-space btree holds.
    CounterAgfFreeblks,
    /// The AGF's longest free extent is not the longest one there is.
    CounterAgfLongest,
    /// The AGF's count of free-space btree blocks is wrong.
    CounterAgfBtreeblks,
    /// The AGI's inode or free inode count is not what the inode btree holds.
    CounterAgiInodes,
    /// The AGI's count of inode btree blocks is wrong.
    CounterAgiIblocks,
    /// The AGI's count of free inode btree blocks is wrong.
    CounterAgiFblocks,
    /// The superblock's inode count is not what the groups add up to.
    CounterSbIcount,
    /// The superblock's free inode count is not what the groups add up to.
    CounterSbIfree,
    /// The superblock's free block count is not what the groups add up to.
    CounterSbFdblocks,
    /// Blocks are claimed outside the group, or outside any group.
    RangeBlock,
    /// An inode maps an extent outside one allocation group.
    RangeExtent,
    /// A block is claimed by two owners.
    CrossLink,
    /// Blocks are claimed by nothing.
    Lost,
    /// An inode's extent list or extent tree could not be read.
    ExtentUnreadable,
    /// An inode could not be read.
    InodeUnreadable,
    /// An inode the inode btree calls free is in use.
    InodeFreeInUse,
    /// An inode the inode btree calls allocated is not in use.
    InodeAllocatedUnused,
    /// An inode's link count is not the number of entries that reach it.
    InodeNlink,
    /// The root directory is missing or unreadable.
    DirRoot,
    /// An inode reached as a directory is not one.
    DirNotADirectory,
    /// A directory's entries could not be read.
    DirUnreadable,
    /// A directory entry points at an inode that is not in use.
    DirEntryTarget,
    /// A directory entry's recorded type is not its inode's.
    DirEntryFtype,
    /// A directory is reached from more than one place.
    DirReachedTwice,
    /// An allocated inode is reached by no directory.
    DirUnreached,
    /// The log held records that had not been applied; the volume was
    /// checked as replaying them leaves it, and its counters were not.
    LogReplayed,
    /// The filesystem could not be mounted, so nothing was checked.
    Mount,
    /// The superblock's realtime fields disagree with each other (#381).
    RtGeometry,
    /// A realtime file maps an extent outside the realtime section.
    RtExtent,
    /// Two realtime files map one realtime extent.
    RtCrossLink,
    /// The realtime bitmap calls an extent free that a file maps, or in use
    /// when nothing maps it.
    RtBitmap,
    /// The realtime summary is not what the bitmap adds up to.
    RtSummary,
    /// The superblock's free realtime extent count is not what the bitmap
    /// holds.
    CounterSbFrextents,
    /// A block has an owner the reverse-mapping btree does not record (#380).
    RmapMissing,
    /// The reverse-mapping btree records an owner for a block nothing
    /// owns, or that is free.
    RmapStale,
    /// The reverse-mapping btree records a block under one owner while the
    /// walk found it owned by another.
    RmapOwner,
    /// The reverse-mapping btree records the same owner of a block twice.
    RmapDuplicate,
    /// The refcount btree records blocks as shared that one owner or none
    /// holds (#379).
    RefcountStale,
    /// The refcount btree's count for shared blocks is not the number of
    /// file mappings the walk finds.
    RefcountCount,
    /// No repair was planned: another holder has the target, or it is
    /// mounted or attached to a loop device (#375).
    RepairNotExclusive,
    /// No repair was planned: the volume uses a feature whose metadata
    /// the planner does not reason about.
    RepairFeature,
    /// No repair was planned: the log held records that had not been
    /// applied, so the disk is not what a repair would read.
    RepairLogDirty,
    /// No repair was planned: the check did not cover the whole volume,
    /// or a rule could not finish its plan.
    RepairIncomplete,
    /// No repair was planned: two structures claim the same block, or a
    /// directory is reached from two places, so there is no single owner
    /// to repair towards.
    RepairAmbiguous,
    /// The superblock's quota flags are not valid for its version (#395).
    QuotaFlags,
    /// A quota inode is missing, shared between quota types, not a quota
    /// file, maps blocks a quota file cannot have, or holds a record that
    /// fails its checksum, UUID, identity or field checks.
    QuotaInode,
    /// A quota inode's extent map or records, or the usage they are checked
    /// against, could not be read.
    QuotaUnreadable,
    /// Quota accounting is not what the allocated inodes add up to.
    QuotaUsage,
}

impl Code {
    /// Every code, in the order `docs/fsck-output.md` lists them.
    pub const ALL: &'static [Code] = &[
        Code::Checksum,
        Code::Identity,
        Code::SbCopyUnreadable,
        Code::SbCopyField,
        Code::SbCopyUuid,
        Code::AgfUnreadable,
        Code::AgiUnreadable,
        Code::AgflUnreadable,
        Code::AgLength,
        Code::BtreeUnreadable,
        Code::InobtRecord,
        Code::InobtChunkCount,
        Code::FinobtMismatch,
        Code::FreespEmpty,
        Code::FreespOverlap,
        Code::FreespCntOrder,
        Code::FreespDisagree,
        Code::CounterAgfFreeblks,
        Code::CounterAgfLongest,
        Code::CounterAgfBtreeblks,
        Code::CounterAgiInodes,
        Code::CounterAgiIblocks,
        Code::CounterAgiFblocks,
        Code::CounterSbIcount,
        Code::CounterSbIfree,
        Code::CounterSbFdblocks,
        Code::RangeBlock,
        Code::RangeExtent,
        Code::CrossLink,
        Code::Lost,
        Code::ExtentUnreadable,
        Code::InodeUnreadable,
        Code::InodeFreeInUse,
        Code::InodeAllocatedUnused,
        Code::InodeNlink,
        Code::DirRoot,
        Code::DirNotADirectory,
        Code::DirUnreadable,
        Code::DirEntryTarget,
        Code::DirEntryFtype,
        Code::DirReachedTwice,
        Code::DirUnreached,
        Code::LogReplayed,
        Code::Mount,
        Code::RtGeometry,
        Code::RtExtent,
        Code::RtCrossLink,
        Code::RtBitmap,
        Code::RtSummary,
        Code::CounterSbFrextents,
        Code::RmapMissing,
        Code::RmapStale,
        Code::RmapOwner,
        Code::RmapDuplicate,
        Code::RefcountStale,
        Code::RefcountCount,
        Code::RepairNotExclusive,
        Code::RepairFeature,
        Code::RepairLogDirty,
        Code::RepairIncomplete,
        Code::RepairAmbiguous,
        Code::QuotaFlags,
        Code::QuotaInode,
        Code::QuotaUnreadable,
        Code::QuotaUsage,
    ];

    /// The code as it appears in the output.
    pub fn as_str(self) -> &'static str {
        match self {
            Code::Checksum => "checksum",
            Code::Identity => "identity",
            Code::SbCopyUnreadable => "sb.copy.unreadable",
            Code::SbCopyField => "sb.copy.field",
            Code::SbCopyUuid => "sb.copy.uuid",
            Code::AgfUnreadable => "ag.agf.unreadable",
            Code::AgiUnreadable => "ag.agi.unreadable",
            Code::AgflUnreadable => "ag.agfl.unreadable",
            Code::AgLength => "ag.length",
            Code::BtreeUnreadable => "btree.unreadable",
            Code::InobtRecord => "inobt.record",
            Code::InobtChunkCount => "inobt.chunk-count",
            Code::FinobtMismatch => "finobt.mismatch",
            Code::FreespEmpty => "freesp.empty",
            Code::FreespOverlap => "freesp.overlap",
            Code::FreespCntOrder => "freesp.cnt-order",
            Code::FreespDisagree => "freesp.disagree",
            Code::CounterAgfFreeblks => "counter.agf.freeblks",
            Code::CounterAgfLongest => "counter.agf.longest",
            Code::CounterAgfBtreeblks => "counter.agf.btreeblks",
            Code::CounterAgiInodes => "counter.agi.inodes",
            Code::CounterAgiIblocks => "counter.agi.iblocks",
            Code::CounterAgiFblocks => "counter.agi.fblocks",
            Code::CounterSbIcount => "counter.sb.icount",
            Code::CounterSbIfree => "counter.sb.ifree",
            Code::CounterSbFdblocks => "counter.sb.fdblocks",
            Code::RangeBlock => "range.block",
            Code::RangeExtent => "range.extent",
            Code::CrossLink => "cross-link",
            Code::Lost => "lost",
            Code::ExtentUnreadable => "extent.unreadable",
            Code::InodeUnreadable => "inode.unreadable",
            Code::InodeFreeInUse => "inode.free-in-use",
            Code::InodeAllocatedUnused => "inode.allocated-unused",
            Code::InodeNlink => "inode.nlink",
            Code::DirRoot => "dir.root",
            Code::DirNotADirectory => "dir.not-a-directory",
            Code::DirUnreadable => "dir.unreadable",
            Code::DirEntryTarget => "dir.entry-target",
            Code::DirEntryFtype => "dir.entry-ftype",
            Code::DirReachedTwice => "dir.reached-twice",
            Code::DirUnreached => "dir.unreached",
            Code::LogReplayed => "log.replayed",
            Code::Mount => "mount",
            Code::RtGeometry => "rt.geometry",
            Code::RtExtent => "rt.extent",
            Code::RtCrossLink => "rt.cross-link",
            Code::RtBitmap => "rt.bitmap",
            Code::RtSummary => "rt.summary",
            Code::CounterSbFrextents => "counter.sb.frextents",
            Code::RmapMissing => "rmap.missing",
            Code::RmapStale => "rmap.stale",
            Code::RmapOwner => "rmap.owner",
            Code::RmapDuplicate => "rmap.duplicate",
            Code::RefcountStale => "refcount.stale",
            Code::RefcountCount => "refcount.count",
            Code::RepairNotExclusive => "repair.not-exclusive",
            Code::RepairFeature => "repair.feature",
            Code::RepairLogDirty => "repair.log-dirty",
            Code::RepairIncomplete => "repair.incomplete",
            Code::RepairAmbiguous => "repair.ambiguous",
            Code::QuotaFlags => "quota.flags",
            Code::QuotaInode => "quota.inode",
            Code::QuotaUnreadable => "quota.unreadable",
            Code::QuotaUsage => "quota.usage",
        }
    }

    /// How bad a finding with this code is.
    pub fn severity(self) -> Severity {
        match self {
            // A refusal to plan says nothing about damage: a clean volume
            // with a feature the planner does not know is still clean.
            Code::LogReplayed
            | Code::RepairNotExclusive
            | Code::RepairFeature
            | Code::RepairLogDirty
            | Code::RepairIncomplete
            | Code::RepairAmbiguous => Severity::Warning,
            _ => Severity::Error,
        }
    }

    /// True when a finding with this code means something could not be
    /// read, so what lies under it went unchecked.
    pub fn stops_the_walk(self) -> bool {
        matches!(
            self,
            Code::Checksum
                | Code::Identity
                | Code::AgfUnreadable
                | Code::AgiUnreadable
                | Code::AgflUnreadable
                | Code::BtreeUnreadable
                | Code::InobtRecord
                | Code::ExtentUnreadable
                | Code::InodeUnreadable
                | Code::DirRoot
                | Code::DirNotADirectory
                | Code::DirUnreadable
                | Code::Mount
                | Code::QuotaUnreadable
        )
    }
}

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Severity {
    /// The volume is damaged. A volume with one is not clean.
    Error,
    /// Worth knowing, and not damage: a volume with only these is clean.
    Warning,
}

impl Severity {
    /// The severity as it appears in the output.
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
        }
    }
}

/// Where a finding is, as far as it is known.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Location {
    /// The allocation group, where it belongs to one.
    pub ag: Option<u32>,
    /// The block within that group, where it concerns one.
    pub agbno: Option<u32>,
    /// The inode, where it concerns one.
    pub ino: Option<u64>,
    /// The field or structure, by its on-disk name, where it names one.
    pub field: Option<&'static str>,
}

/// One thing found wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What kind of thing is wrong.
    pub code: Code,
    /// Where it is.
    pub location: Location,
    /// What is wrong, in words.
    pub what: String,
}

impl Finding {
    /// How bad it is: the code's severity.
    pub fn severity(&self) -> Severity {
        self.code.severity()
    }
}

/// How much of the volume a check covered.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scan {
    /// Every structure the walk reached was read.
    #[default]
    Complete,
    /// Something could not be read, so what lies under it was not
    /// checked: a volume with no other finding may still be damaged.
    Partial,
}

impl Scan {
    /// The scan as it appears in the output.
    pub fn as_str(self) -> &'static str {
        match self {
            Scan::Complete => "complete",
            Scan::Partial => "partial",
        }
    }
}

/// What a check found.
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// Everything found wrong, in the order it was found, up to
    /// [`PER_KIND`] of one code in one group.
    pub findings: Vec<Finding>,
    /// Findings found and not listed, past [`PER_KIND`] of one code in
    /// one group, so a wholesale corruption does not print a million lines.
    pub suppressed: u64,
    /// Whether everything reached was read.
    pub scan: Scan,
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
    /// Keep reporting bounded without losing whether the scan finished.
    fn record(&mut self, counted: &mut HashMap<(Option<u32>, Code), usize>, finding: Finding) {
        if finding.code.stops_the_walk() {
            self.scan = Scan::Partial;
        }
        let n = counted
            .entry((finding.location.ag, finding.code))
            .or_insert(0);
        *n += 1;
        if *n <= PER_KIND {
            self.findings.push(finding);
        } else {
            self.suppressed += 1;
        }
    }

    /// True when nothing was found wrong: no finding is an error.
    pub fn is_clean(&self) -> bool {
        self.scan == Scan::Complete
            && self.suppressed == 0
            && self
                .findings
                .iter()
                .all(|f| f.severity() != Severity::Error)
    }
}

/// The most findings listed of one code in one group.
pub const PER_KIND: usize = 20;

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
    /// Per (group, code), how many findings were reported.
    counted: HashMap<(Option<u32>, Code), usize>,
    /// Every inode the inode btrees call allocated, and every one they
    /// call free.
    allocated: HashSet<u64>,
    free: HashSet<u64>,
    /// Every realtime extent a file maps: realtime start block, length in
    /// blocks, and the inode (#381).
    rt_used: Vec<(u64, u64, u64)>,
    /// Per group, every claim made, as `(start, length, owner)`, for the
    /// reverse-mapping comparison (#380).
    claimed: Vec<Vec<(u32, u32, Owner)>>,
    /// Per group, the reverse-mapping btree's records, when it was read.
    rmaps: Vec<Option<Vec<crate::rmap::Rmap>>>,
    /// Per group, the refcount btree's records, when it was read.
    refcounts: Vec<Option<Vec<crate::refcount::Refcount>>>,
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
        rt_used: Vec::new(),
        claimed: vec![Vec::new(); sb.agcount as usize],
        rmaps: vec![None; sb.agcount as usize],
        refcounts: vec![None; sb.agcount as usize],
    };
    c.secondaries();
    let mut totals = Totals::default();
    for ag in 0..sb.agcount {
        c.group(ag, &mut totals);
    }
    c.inodes();
    c.unclaimed();
    c.counters(&totals);
    c.realtime();
    c.reverse_mappings();
    c.reference_counts();
    c.report
}

/// Sorted extent numbers as runs of consecutive ones.
fn extent_runs(sorted: &[u64]) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &x in sorted {
        match out.last_mut() {
            Some((start, len)) if *start + *len == x => *len += 1,
            _ => out.push((x, 1)),
        }
    }
    out
}

/// Which fork of its owner a reverse mapping describes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Fork {
    Data,
    Attr,
    ExtentTree,
}

/// An owner as the reverse-mapping btree records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct RmapKey {
    owner: i64,
    fork: Fork,
}

impl RmapKey {
    fn describe(self) -> String {
        use crate::rmap::{OWN_AG, OWN_FS, OWN_INOBT, OWN_INODES, OWN_LOG, OWN_REFC};
        match (self.owner, self.fork) {
            (OWN_FS, _) => "the group headers".into(),
            (OWN_LOG, _) => "the log".into(),
            (OWN_AG, _) => "the free-space trees and free list".into(),
            (OWN_INOBT, _) => "the inode btrees".into(),
            (OWN_INODES, _) => "an inode chunk".into(),
            (OWN_REFC, _) => "the refcount btree".into(),
            (ino, Fork::Data) if ino >= 0 => format!("inode {ino}'s data"),
            (ino, Fork::Attr) if ino >= 0 => format!("inode {ino}'s attributes"),
            (ino, Fork::ExtentTree) if ino >= 0 => format!("inode {ino}'s extent tree"),
            (other, _) => format!("reserved owner {other}"),
        }
    }
}

/// The reverse-mapping owner of what the walk claimed, or `None` for free
/// or unclaimed space, which no record describes.
fn rmap_key(owner: Owner) -> Option<RmapKey> {
    use crate::rmap::{OWN_AG, OWN_FS, OWN_INOBT, OWN_INODES, OWN_LOG, OWN_REFC};
    let (owner, fork) = match owner {
        Owner::Unclaimed | Owner::Free => return None,
        Owner::Headers => (OWN_FS, Fork::Data),
        Owner::Log => (OWN_LOG, Fork::Data),
        Owner::FreeList => (OWN_AG, Fork::Data),
        Owner::Btree("refcount") => (OWN_REFC, Fork::Data),
        Owner::Btree("inode") | Owner::Btree("free inode") => (OWN_INOBT, Fork::Data),
        Owner::Btree(_) => (OWN_AG, Fork::Data),
        Owner::Inodes => (OWN_INODES, Fork::Data),
        Owner::Data(ino) => (i64::try_from(ino).ok()?, Fork::Data),
        Owner::Attr(ino) => (i64::try_from(ino).ok()?, Fork::Attr),
        Owner::ExtentTree(ino) => (i64::try_from(ino).ok()?, Fork::ExtentTree),
    };
    Some(RmapKey { owner, fork })
}

/// The fork a reverse mapping's offset flags name.
fn fork_of(flags: u64) -> Fork {
    if flags & crate::rmap::OFF_BMBT_BLOCK != 0 {
        Fork::ExtentTree
    } else if flags & crate::rmap::OFF_ATTR_FORK != 0 {
        Fork::Attr
    } else {
        Fork::Data
    }
}

/// Sorted `(block, key)` pairs as runs of consecutive blocks with one key.
fn runs(blocks: &[(u32, RmapKey)]) -> Vec<(u32, u32, RmapKey)> {
    let mut sorted = blocks.to_vec();
    sorted.sort_unstable_by_key(|&(b, k)| (k, b));
    let mut out: Vec<(u32, u32, RmapKey)> = Vec::new();
    for (b, key) in sorted {
        match out.last_mut() {
            Some((start, len, k)) if *k == key && *start + *len == b => *len += 1,
            _ => out.push((b, 1, key)),
        }
    }
    out.sort_unstable_by_key(|&(start, _, _)| start);
    out
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
    fn find(&mut self, code: Code, ag: Option<u32>, ino: Option<u64>, what: String) {
        let location = Location {
            ag,
            ino,
            ..Location::default()
        };
        self.find_at(code, location, what);
    }

    fn find_at(&mut self, code: Code, mut location: Location, what: String) {
        // An inode is in one group, whether or not the check that found
        // it was walking that group.
        if let (None, Some(ino)) = (location.ag, location.ino) {
            location.ag = Some(self.fs.superblock().split_ino(ino).0);
        }
        self.report.record(
            &mut self.counted,
            Finding {
                code,
                location,
                what,
            },
        );
    }

    /// Something could not be read. A checksum or a self-describing
    /// header that names another place is reported as that, whatever was
    /// being read; anything else as `code`.
    fn failed(
        &mut self,
        code: Code,
        e: &crate::error::Error,
        ag: Option<u32>,
        ino: Option<u64>,
        what: String,
    ) {
        let code = match e {
            crate::error::Error::ChecksumMismatch { .. } => Code::Checksum,
            crate::error::Error::BlockIdentityMismatch { .. } => Code::Identity,
            _ => code,
        };
        self.find(code, ag, ino, what);
    }

    /// Claim `len` blocks at `start` in group `ag` for `owner`.
    fn claim(&mut self, ag: u32, start: u32, len: u32, owner: Owner) {
        let Some(map) = self.owners.get_mut(ag as usize) else {
            self.find(
                Code::RangeBlock,
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
            let location = Location {
                ag: Some(ag),
                agbno: Some(start),
                ino: owner_ino(owner),
                field: None,
            };
            self.find_at(Code::RangeBlock, location, what);
            return;
        }
        self.claimed[ag as usize].push((start, len, owner));
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
            let location = Location {
                ag: Some(ag),
                agbno: Some(b),
                ino: owner_ino(owner),
                field: None,
            };
            self.find_at(Code::CrossLink, location, what);
        }
    }

    fn claim_fsblocks(&mut self, fsblock: u64, len: u64, owner: Owner) {
        let sb = self.fs.superblock();
        let (ag, agbno) = sb.split_fsblock(fsblock);
        match u32::try_from(len) {
            Ok(len) => self.claim(ag, agbno, len, owner),
            Err(_) => self.find(
                Code::RangeExtent,
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
                    Code::SbCopyUnreadable,
                    Some(ag),
                    None,
                    format!("reading the superblock copy: {e}"),
                );
                continue;
            }
            // A copy that fails its checksum is this code too, not
            // `checksum`: nothing is reached through a copy, so it is the
            // copy alone that went unchecked, and a repair from the primary
            // owns it (#391).
            let copy = match crate::superblock::Superblock::parse_copy(&raw) {
                Ok(copy) => copy,
                Err(e) => {
                    self.find(
                        Code::SbCopyUnreadable,
                        Some(ag),
                        None,
                        format!("the superblock copy: {e}"),
                    );
                    continue;
                }
            };
            let fields: [(&'static str, u64, u64); 12] = [
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
                    let location = Location {
                        ag: Some(ag),
                        field: Some(name),
                        ..Location::default()
                    };
                    self.find_at(
                        Code::SbCopyField,
                        location,
                        format!("the superblock copy says {name} {secondary}; the primary says {primary}"),
                    );
                }
            }
            if copy.uuid != sb.uuid {
                self.find(
                    Code::SbCopyUuid,
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
                self.failed(
                    Code::AgfUnreadable,
                    &e,
                    Some(ag),
                    None,
                    format!("the AGF: {e}"),
                );
                return;
            }
        };
        let agi = match fs.read_agi(ag) {
            Ok(agi) => agi,
            Err(e) => {
                self.failed(
                    Code::AgiUnreadable,
                    &e,
                    Some(ag),
                    None,
                    format!("the AGI: {e}"),
                );
                return;
            }
        };
        if agf.length != ag_length(fs, ag) || agi.length != agf.length {
            self.find(
                Code::AgLength,
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
            Err(e) => self.failed(
                Code::AgflUnreadable,
                &e,
                Some(ag),
                None,
                format!("the free list: {e}"),
            ),
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
                    for r in &records {
                        if r.refcount > 1 && !r.cow {
                            for b in r.startblock..r.startblock.saturating_add(r.blockcount) {
                                self.shared[ag as usize].insert(b);
                            }
                        }
                    }
                    self.refcounts[ag as usize] = Some(records);
                }
                Err(e) => self.failed(
                    Code::BtreeUnreadable,
                    &e,
                    Some(ag),
                    None,
                    format!("the refcount btree: {e}"),
                ),
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
                Err(e) => self.failed(
                    Code::BtreeUnreadable,
                    &e,
                    Some(ag),
                    None,
                    format!("the {name} btree: {e}"),
                ),
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
                crate::rmap::decode,
            ) {
                Ok((records, blocks)) => {
                    btree_blocks += blocks.len() as u32 - 1;
                    for b in blocks {
                        self.claim(ag, b, 1, Owner::Btree("reverse mapping"));
                    }
                    self.rmaps[ag as usize] = Some(records);
                }
                Err(e) => self.failed(
                    Code::BtreeUnreadable,
                    &e,
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
                        Code::CounterAgiIblocks,
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
                        Err(e) => self.failed(
                            Code::InobtRecord,
                            &e,
                            Some(ag),
                            None,
                            format!("an inode btree record: {e}"),
                        ),
                    }
                }
                chunks
            }
            Err(e) => {
                self.failed(
                    Code::BtreeUnreadable,
                    &e,
                    Some(ag),
                    None,
                    format!("the inode btree: {e}"),
                );
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
                            Code::CounterAgiFblocks,
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
                            Code::FinobtMismatch,
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
                Err(e) => self.failed(
                    Code::BtreeUnreadable,
                    &e,
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
                    Code::FreespEmpty,
                    Some(ag),
                    None,
                    format!("free extent {i} is empty"),
                );
            }
            if i > 0 && u64::from(e.startblock) < end {
                self.find(
                    Code::FreespOverlap,
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
                    Code::FreespCntOrder,
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
                Code::FreespDisagree,
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
                Code::CounterAgfFreeblks,
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
                Code::CounterAgfLongest,
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
                Code::CounterAgfBtreeblks,
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
                    Code::InobtChunkCount,
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
                Code::CounterAgiInodes,
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
                    Code::InodeFreeInUse,
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
                            Code::InodeAllocatedUnused,
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
                Err(e) => self.failed(
                    Code::InodeUnreadable,
                    &e,
                    Some(ag),
                    Some(ino),
                    format!("inode {ino}: {e}"),
                ),
            }
        }

        self.quotas(&usable, &raws);

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
                Code::DirRoot,
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
                    Code::DirNotADirectory,
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
                    self.failed(
                        Code::DirUnreadable,
                        &e,
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
                        Code::DirEntryTarget,
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
                        Code::DirEntryFtype,
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
                            Code::DirReachedTwice,
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
                    Code::DirUnreached,
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
                    Code::InodeNlink,
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
        let (start, end) = inode.data_fork_range(isz);
        if inode.is_realtime() {
            self.realtime_fork(ino, inode.format, &raw[start..end], inode.nextents);
        } else {
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
                            Code::RangeExtent,
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
            Err(e) => self.failed(
                Code::ExtentUnreadable,
                &e,
                Some(ag),
                Some(ino),
                format!("inode {ino}'s extents: {e}"),
            ),
        }
    }

    /// A realtime file's data fork (#381): its extent tree, if it has one,
    /// is on the data device and claimed like any other; its extents are
    /// on the realtime device and are recorded for [`Checker::realtime`].
    fn realtime_fork(&mut self, ino: u64, format: Format, fork: &[u8], nextents: u64) {
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
                    let inside = e
                        .startblock
                        .checked_add(e.blockcount)
                        .is_some_and(|end| e.blockcount > 0 && end <= sb.rblocks);
                    if inside {
                        self.rt_used.push((e.startblock, e.blockcount, ino));
                    } else {
                        self.find(
                            Code::RtExtent,
                            Some(ag),
                            Some(ino),
                            format!(
                                "realtime inode {ino} maps {} blocks at realtime block {}, \
                                 outside the {}-block realtime section",
                                e.blockcount, e.startblock, sb.rblocks
                            ),
                        );
                    }
                }
            }
            Err(e) => self.failed(
                Code::ExtentUnreadable,
                &e,
                Some(ag),
                Some(ino),
                format!("realtime inode {ino}'s extents: {e}"),
            ),
        }
    }

    /// The realtime section's geometry, bitmap, summary and free count
    /// against the extents realtime files map (#381).
    ///
    /// The bitmap holds one bit per realtime extent, set when the extent
    /// is free. The summary counts, for each power of two and each bitmap
    /// block, the free runs of that size class that start in that block.
    /// Both are read through their inodes on the data device, so no
    /// realtime device is needed to check them.
    ///
    /// THE WORDS ARE LITTLE-ENDIAN HERE. Before realtime groups, both files
    /// were written in the host's own byte order, an `xfs_rtword_t` and a
    /// `__u32` with no conversion. Every volume this crate is validated
    /// against was made on a little-endian host, and so is every host it
    /// targets; a big-endian one's volume would read as damaged here, not
    /// as clean.
    fn realtime(&mut self) {
        let sb = self.fs.superblock().clone();
        if sb.rblocks == 0 {
            return;
        }
        let bs = u64::from(sb.blocksize);
        let rextsize = u64::from(sb.rextsize);
        let geometry = |what: String, field: &'static str| Finding {
            code: Code::RtGeometry,
            location: Location {
                field: Some(field),
                ..Location::default()
            },
            what,
        };
        let mut broken = Vec::new();
        if rextsize == 0 || !(4096..=1 << 30).contains(&(rextsize * bs)) {
            broken.push(geometry(
                format!("a realtime extent of {rextsize} blocks is outside 4 KiB..1 GiB"),
                "sb_rextsize",
            ));
        } else {
            let rextents = sb.rblocks / rextsize;
            if sb.rextents != rextents {
                broken.push(geometry(
                    format!(
                        "sb_rextents is {}, and {} blocks of {rextsize} make {rextents}",
                        sb.rextents, sb.rblocks
                    ),
                    "sb_rextents",
                ));
            }
            let log = if rextents == 0 {
                0
            } else {
                63 - rextents.leading_zeros()
            };
            if u32::from(sb.rextslog) != log {
                broken.push(geometry(
                    format!(
                        "sb_rextslog is {}, and {rextents} extents need {log}",
                        sb.rextslog
                    ),
                    "sb_rextslog",
                ));
            }
            let bitmap_blocks = rextents.div_ceil(bs * 8);
            if u64::from(sb.rbmblocks) != bitmap_blocks {
                broken.push(geometry(
                    format!(
                        "sb_rbmblocks is {}, and a bitmap of {rextents} extents needs \
                         {bitmap_blocks} blocks",
                        sb.rbmblocks
                    ),
                    "sb_rbmblocks",
                ));
            }
        }
        if sb.rbmino == 0 || sb.rsumino == 0 || sb.rbmino == sb.rsumino {
            broken.push(geometry(
                format!(
                    "the realtime bitmap inode {} and summary inode {} are not two inodes",
                    sb.rbmino, sb.rsumino
                ),
                "sb_rbmino",
            ));
        }
        let geometry_ok = broken.is_empty();
        for f in broken {
            self.find_at(f.code, f.location, f.what);
        }
        if !geometry_ok {
            return;
        }

        let rextents = sb.rextents as usize;
        let mut users = vec![0u8; rextents];
        let mut user_of = vec![0u64; rextents];
        let mut crossed = Vec::new();
        for &(start, len, ino) in &self.rt_used {
            for x in start / rextsize..=(start + len - 1) / rextsize {
                let x = x as usize;
                if users[x] > 0 && user_of[x] != ino {
                    crossed.push((x as u64, ino));
                }
                users[x] = users[x].saturating_add(1);
                user_of[x] = ino;
            }
        }
        for (x, ino) in crossed.into_iter().take(PER_KIND) {
            self.find(
                Code::RtCrossLink,
                None,
                Some(ino),
                format!("realtime extent {x} is mapped by inode {ino} and another file"),
            );
        }

        let read = |ino: u64| -> Result<Vec<u8>> {
            let (inode, raw) = self.fs.read_inode_raw(ino)?;
            self.fs.read_file(&inode, &raw)
        };
        let bitmap = match read(sb.rbmino) {
            Ok(b) => b,
            Err(e) => {
                self.failed(
                    Code::InodeUnreadable,
                    &e,
                    None,
                    Some(sb.rbmino),
                    format!("the realtime bitmap inode {}: {e}", sb.rbmino),
                );
                return;
            }
        };
        let words_needed = rextents.div_ceil(32);
        if bitmap.len() < words_needed * 4 {
            self.find(
                Code::RtBitmap,
                None,
                Some(sb.rbmino),
                format!(
                    "the realtime bitmap holds {} bytes, too few for {rextents} extents",
                    bitmap.len()
                ),
            );
            return;
        }
        let free_at = |x: usize| -> bool {
            let w = u32::from_le_bytes(bitmap[x / 32 * 4..x / 32 * 4 + 4].try_into().expect("4"));
            w >> (x % 32) & 1 == 1
        };
        let mut free_count = 0u64;
        let (mut marked_free_used, mut marked_used_free) = (Vec::new(), Vec::new());
        for (x, &mapped_by) in users.iter().enumerate() {
            let free = free_at(x);
            if free {
                free_count += 1;
            }
            match (free, mapped_by > 0) {
                (true, true) => marked_free_used.push(x as u64),
                (false, false) => marked_used_free.push(x as u64),
                _ => {}
            }
        }
        for (blocks, words) in [
            (
                marked_free_used,
                "are free in the realtime bitmap and mapped by a file",
            ),
            (
                marked_used_free,
                "are in use in the realtime bitmap and mapped by nothing",
            ),
        ] {
            for (start, len) in extent_runs(&blocks) {
                self.find(
                    Code::RtBitmap,
                    None,
                    None,
                    format!("realtime extents {start}..{} {words}", start + len),
                );
            }
        }
        if !self.report.dirty && free_count != sb.frextents {
            self.find_at(
                Code::CounterSbFrextents,
                Location {
                    field: Some("sb_frextents"),
                    ..Location::default()
                },
                format!(
                    "sb_frextents is {}, and the realtime bitmap holds {free_count} free extents",
                    sb.frextents
                ),
            );
        }

        // The summary: a count per (size class, bitmap block) of the free
        // runs whose first extent lies in that bitmap block.
        let levels = usize::from(sb.rextslog) + 1;
        let bitmap_blocks = sb.rbmblocks as usize;
        let mut want = vec![0u32; levels * bitmap_blocks];
        let per_block = (bs * 8) as usize;
        let mut x = 0;
        while x < rextents {
            if !free_at(x) {
                x += 1;
                continue;
            }
            let start = x;
            while x < rextents && free_at(x) {
                x += 1;
            }
            let len = (x - start) as u64;
            let log = (63 - len.leading_zeros()) as usize;
            if log < levels {
                want[log * bitmap_blocks + start / per_block] += 1;
            }
        }
        let summary = match read(sb.rsumino) {
            Ok(s) => s,
            Err(e) => {
                self.failed(
                    Code::InodeUnreadable,
                    &e,
                    None,
                    Some(sb.rsumino),
                    format!("the realtime summary inode {}: {e}", sb.rsumino),
                );
                return;
            }
        };
        if summary.len() < want.len() * 4 {
            self.find(
                Code::RtSummary,
                None,
                Some(sb.rsumino),
                format!(
                    "the realtime summary holds {} bytes, too few for {levels} size classes \
                     over {bitmap_blocks} bitmap blocks",
                    summary.len()
                ),
            );
            return;
        }
        let mut wrong = 0;
        for (i, &w) in want.iter().enumerate() {
            let got = u32::from_le_bytes(summary[i * 4..i * 4 + 4].try_into().expect("4"));
            if got != w {
                if wrong < PER_KIND {
                    self.find(
                        Code::RtSummary,
                        None,
                        Some(sb.rsumino),
                        format!(
                            "the realtime summary counts {got} free runs of 2^{} extents \
                             starting in bitmap block {}, and the bitmap holds {w}",
                            i / bitmap_blocks,
                            i % bitmap_blocks
                        ),
                    );
                }
                wrong += 1;
            }
        }
    }

    /// A block nothing claimed is lost.
    /// Every group's reverse-mapping records against the owners the walk
    /// found (#380).
    ///
    /// Both sides are reduced to `(block, owner, fork)` and compared block
    /// by block: a block the walk found owned and the btree does not
    /// record is missing, a record for a block nothing owns is stale, a
    /// block both sides describe with different owners has the wrong
    /// owner, and a record repeated for one block is a duplicate.
    /// Consecutive blocks with the same verdict are one finding. Nothing is
    /// compared after a partial scan, whose claims are not all made.
    fn reverse_mappings(&mut self) {
        if self.report.scan != Scan::Partial {
            for ag in 0..self.rmaps.len() {
                if let Some(records) = self.rmaps[ag].take() {
                    self.reverse_mapping_group(ag as u32, &records);
                }
            }
        }
    }

    /// Every group's refcount records against the file mappings the walk
    /// found (#379).
    ///
    /// A block's count is the number of file mappings that claim it, one
    /// per extent, so one inode mapping a block twice counts twice, as the
    /// kernel counts it. A record over blocks that one mapping or none
    /// holds is stale; a record whose count differs from the walk's is
    /// wrong. A shared block with no record at all is already a
    /// cross-link. Copy-on-write staging records are left alone, and
    /// nothing is compared after a partial scan.
    fn reference_counts(&mut self) {
        if self.report.scan == Scan::Partial {
            return;
        }
        for ag in 0..self.refcounts.len() {
            let Some(records) = self.refcounts[ag].take() else {
                continue;
            };
            let mut mappings: HashMap<u32, u32> = HashMap::new();
            for &(start, len, owner) in &self.claimed[ag] {
                if owner.is_file_data() {
                    for b in start..start + len {
                        *mappings.entry(b).or_insert(0) += 1;
                    }
                }
            }
            let ag = ag as u32;
            for r in records.iter().filter(|r| !r.cow) {
                let end = r.startblock.saturating_add(r.blockcount);
                let mut b = r.startblock;
                while b < end {
                    let found = mappings.get(&b).copied().unwrap_or(0);
                    let mut run = 1;
                    while b + run < end && mappings.get(&(b + run)).copied().unwrap_or(0) == found {
                        run += 1;
                    }
                    let verdict = if found <= 1 {
                        Some(Code::RefcountStale)
                    } else if found != r.refcount {
                        Some(Code::RefcountCount)
                    } else {
                        None
                    };
                    if let Some(code) = verdict {
                        self.find_at(
                            code,
                            Location {
                                ag: Some(ag),
                                agbno: Some(b),
                                ino: None,
                                field: None,
                            },
                            format!(
                                "blocks {b}..{} of group {ag} are recorded as shared by {} \
                                 mappings, and the walk finds {found}",
                                b + run,
                                r.refcount
                            ),
                        );
                    }
                    b += run;
                }
            }
        }
    }

    fn reverse_mapping_group(&mut self, ag: u32, records: &[crate::rmap::Rmap]) {
        let length = self.owners[ag as usize].len() as u32;
        let mut expected: Vec<(u32, RmapKey)> = Vec::new();
        for &(start, len, owner) in &self.claimed[ag as usize] {
            if let Some(key) = rmap_key(owner) {
                expected.extend((start..start + len).map(|b| (b, key)));
            }
        }
        expected.sort_unstable();
        expected.dedup();
        let mut actual: Vec<(u32, RmapKey)> = Vec::new();
        for r in records {
            if r.owner == crate::rmap::OWN_COW {
                continue;
            }
            let key = RmapKey {
                owner: r.owner,
                fork: fork_of(r.flags()),
            };
            let end = r.startblock.saturating_add(r.blockcount).min(length);
            actual.extend((r.startblock..end).map(|b| (b, key)));
        }
        actual.sort_unstable();
        let mut duplicates = Vec::new();
        for w in actual.windows(2) {
            if w[0] == w[1] {
                duplicates.push(w[0]);
            }
        }
        actual.dedup();

        let (mut missing, mut extra) = (Vec::new(), Vec::new());
        let (mut i, mut j) = (0, 0);
        while i < expected.len() || j < actual.len() {
            match (expected.get(i), actual.get(j)) {
                (Some(e), Some(a)) if e == a => {
                    i += 1;
                    j += 1;
                }
                (Some(e), Some(a)) if e < a => {
                    missing.push(*e);
                    i += 1;
                }
                (Some(_), Some(a)) | (None, Some(a)) => {
                    extra.push(*a);
                    j += 1;
                }
                (Some(e), None) => {
                    missing.push(*e);
                    i += 1;
                }
                (None, None) => break,
            }
        }
        let missing_blocks: HashSet<u32> = missing.iter().map(|&(b, _)| b).collect();
        let extra_blocks: HashSet<u32> = extra.iter().map(|&(b, _)| b).collect();
        let (mut wrong, mut lost, mut stale) = (Vec::new(), Vec::new(), Vec::new());
        for (b, key) in missing {
            if extra_blocks.contains(&b) {
                wrong.push((b, key));
            } else {
                lost.push((b, key));
            }
        }
        for (b, key) in extra {
            if !missing_blocks.contains(&b) {
                stale.push((b, key));
            }
        }
        let verdicts = [
            (
                Code::RmapMissing,
                lost,
                "owned by {owner} but the reverse-mapping btree does not record it",
            ),
            (
                Code::RmapOwner,
                wrong,
                "owned by {owner} but the reverse-mapping btree records another owner",
            ),
            (
                Code::RmapStale,
                stale,
                "recorded for {owner} by the reverse-mapping btree, which does not own it",
            ),
            (
                Code::RmapDuplicate,
                duplicates,
                "recorded for {owner} more than once by the reverse-mapping btree",
            ),
        ];
        for (code, blocks, words) in verdicts {
            for (start, len, key) in runs(&blocks) {
                let what = words.replace("{owner}", &key.describe());
                self.find_at(
                    code,
                    Location {
                        ag: Some(ag),
                        agbno: Some(start),
                        ino: u64::try_from(key.owner).ok(),
                        field: None,
                    },
                    format!("blocks {start}..{} of group {ag} are {what}", start + len),
                );
            }
        }
    }

    fn unclaimed(&mut self) {
        for ag in 0..self.owners.len() {
            let lost: Vec<u32> = self.owners[ag]
                .iter()
                .enumerate()
                .filter(|(_, o)| **o == Owner::Unclaimed)
                .map(|(b, _)| b as u32)
                .collect();
            if let Some(&first) = lost.first() {
                let location = Location {
                    ag: Some(ag as u32),
                    agbno: Some(first),
                    ..Location::default()
                };
                self.find_at(
                    Code::Lost,
                    location,
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
            self.find(
                Code::LogReplayed,
                None,
                None,
                "the log held unapplied records: checked as replayed, and the superblock's \
                 counters, which the kernel recomputes at mount, were not compared"
                    .into(),
            );
            return;
        }
        let sb = self.fs.superblock().clone();
        for (code, name, said, counted) in [
            (Code::CounterSbIcount, "icount", sb.icount, totals.icount),
            (Code::CounterSbIfree, "ifree", sb.ifree, totals.ifree),
            (
                Code::CounterSbFdblocks,
                "fdblocks",
                sb.fdblocks,
                totals.fdblocks,
            ),
        ] {
            if said != counted {
                self.find(
                    code,
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

pub(crate) fn has_inobt_counts(sb: &crate::superblock::Superblock) -> bool {
    sb.features_ro_compat & crate::superblock::ro_compat::INOBTCNT != 0
}

pub(crate) fn has_lazy_counters(sb: &crate::superblock::Superblock) -> bool {
    sb.features2 & crate::superblock::features2_flags::LAZYSBCOUNT != 0
}

/// `sb_versionnum` bits the kernel sets in the primary superblock alone,
/// on first use: attributes and quotas.
const LATE_VERSION_BITS: u16 =
    crate::superblock::version_flags::ATTRBIT | crate::superblock::version_flags::QUOTABIT;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finding_limits_preserve_scan_status_and_count_by_code_and_group() {
        let mut report = Report::default();
        let mut counted = HashMap::new();
        let mut finding = Finding {
            code: Code::InodeNlink,
            location: Location {
                ag: Some(0),
                ino: Some(128),
                ..Location::default()
            },
            what: "link count differs".into(),
        };
        for _ in 0..PER_KIND + 3 {
            report.record(&mut counted, finding.clone());
        }
        assert_eq!(report.findings.len(), PER_KIND);
        assert_eq!(report.suppressed, 3);
        assert_eq!(report.scan, Scan::Complete);
        finding.location.ag = Some(1);
        report.record(&mut counted, finding.clone());
        finding.code = Code::Checksum;
        for _ in 0..PER_KIND + 1 {
            report.record(&mut counted, finding.clone());
        }
        assert_eq!(report.findings.len(), PER_KIND * 2 + 1);
        assert_eq!(report.suppressed, 4);
        assert_eq!(report.scan, Scan::Partial);
        assert!(!report.is_clean());
        assert_eq!(report.findings.last().unwrap().location.ino, Some(128));
    }

    #[test]
    fn finding_codes_are_unique_and_documented() {
        let mut seen = HashSet::new();
        let docs = include_str!("../docs/fsck-output.md");
        for &code in Code::ALL {
            assert!(seen.insert(code.as_str()), "duplicate code {code:?}");
            let row = format!(
                "| `{}` | {} | {} |",
                code.as_str(),
                code.severity().as_str(),
                if code.stops_the_walk() { "yes" } else { "no" },
            );
            assert!(docs.contains(&row), "missing documented contract: {row}");
        }
        assert_eq!(Scan::Complete.as_str(), "complete");
        assert_eq!(Scan::Partial.as_str(), "partial");
    }

    #[test]
    fn incomplete_or_suppressed_reports_cannot_be_clean() {
        let mut report = Report::default();
        assert!(report.is_clean());
        report.scan = Scan::Partial;
        assert!(
            !report.is_clean(),
            "an empty incomplete report is not clean"
        );
        report.scan = Scan::Complete;
        report.suppressed = 1;
        assert!(!report.is_clean(), "omitted findings cannot imply clean");
    }

    #[test]
    fn warnings_preserve_clean_only_for_complete_reports() {
        let mut report = Report {
            findings: vec![Finding {
                code: Code::LogReplayed,
                location: Location::default(),
                what: "replayed in memory".into(),
            }],
            ..Report::default()
        };
        assert_eq!(report.findings[0].severity(), Severity::Warning);
        assert!(report.is_clean());
        report.scan = Scan::Partial;
        assert!(!report.is_clean());
        report.scan = Scan::Complete;
        report.findings[0].code = Code::Checksum;
        assert_eq!(report.findings[0].severity(), Severity::Error);
        assert!(!report.is_clean());
    }
}
