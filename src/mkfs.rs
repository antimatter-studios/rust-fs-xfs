//! Making a new v5 filesystem (#338).
//!
//! Every other write path in this crate edits a filesystem that already
//! exists. This one builds the initial layout: the superblock and its
//! copies, each allocation group's headers and empty btrees, the
//! free-list blocks, the inode chunk holding the root directory and the
//! two realtime inodes, and an initialised log.
//!
//! # Where the layout comes from
//!
//! **Measurement.** Every value below was read off volumes the standard
//! formatter made, at 1 KiB, 2 KiB and 4 KiB blocks, and is written the
//! way that tool writes it: where each structure sits, what each header
//! holds, which fields of the secondary superblocks differ from the
//! primary, the one record an initialised log contains. A fresh volume
//! that differs from the standard formatter's only in its UUID and its
//! timestamps is one every reader already understands, and one
//! `xfs_repair` has no opinion about.
//!
//! The layout of one allocation group, in filesystem blocks:
//!
//! ```text
//!  0        superblock copy, AGF, AGI, AGFL (four sectors; two blocks at 1 KiB)
//!  +0..+4   roots: free space by block, by count, inodes, free inodes, refcount
//!  [log]    the internal log, in the middle group only
//!  +0..+3   four blocks on the free list
//!  [chunk]  group 0 only: the first inode chunk, aligned up to sb_inoalignmt
//!  ...      free space
//! ```
//!
//! The root inode number is not a choice. `xfs_repair` computes where the
//! first chunk must be from the geometry — the headers, the five roots,
//! the minimum free list and (when it is in group 0) the log, rounded up
//! to the inode alignment — and calls any other root inode a corruption.
//! [`Plan`] puts the chunk exactly there.
//!
//! # What it makes
//!
//! The standard formatter's defaults: CRCs, the free inode btree, sparse
//! inode chunks, reflink (an empty refcount btree per group), the inode
//! btree block counters, large timestamps and directory entry file types.
//! No reverse-mapping btree, no realtime device, 512-byte sectors and
//! 512-byte inodes. Block sizes of 1, 2 and 4 KiB: those are the ones
//! measured and tested.
//!
//! # What it refuses
//!
//! A device under 300 MiB, as the standard formatter does: below that the
//! 64 MiB log it insists on (and the kernel expects) does not leave room
//! for a filesystem worth having.

use crate::ag::{offsets as ago, XFS_AGFL_MAGIC, XFS_AGF_MAGIC, XFS_AGI_MAGIC};
use crate::ag_btree::offsets as bto;
use crate::agfl::NULLAGBLOCK;
use crate::alloc_btree::{XFS_ABTB_CRC_MAGIC, XFS_ABTC_CRC_MAGIC};
use crate::error::{Error, Result};
use crate::inode::offsets as io;
use crate::inode_btree::{XFS_FIBT_CRC_MAGIC, XFS_IBT_CRC_MAGIC};
use crate::refcount::XFS_REFC_CRC_MAGIC;
use crate::superblock::{
    crc32c_with_zeroed_crc, features2_flags, incompat, ro_compat, version_flags, Superblock,
};
use fs_core::{BlockDevice, BlockRead};

/// The default block size, and the largest this formatter makes.
pub const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// The smallest block size a v5 filesystem may have.
pub const MIN_BLOCK_SIZE: u32 = 1024;
/// The largest block size this formatter makes: the largest measured and
/// tested, and the largest every kernel mounts on a 4 KiB page.
pub const MAX_BLOCK_SIZE: u32 = 4096;
/// `sb_fname` is twelve bytes and needs no terminator.
pub const MAX_LABEL_BYTES: usize = 12;
/// The smallest device this formatter (and the standard one) formats.
pub const MIN_DEVICE_BYTES: u64 = 300 * MIB;

/// `sb_versionnum` bits the superblock module has no name for.
const EXTFLGBIT: u16 = 0x1000;
const DIRV2BIT: u16 = 0x2000;
/// `sb_features2`: metadata checksums.
const CRCBIT: u32 = 0x0000_0100;

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

const SECTOR: u32 = 512;
const INODE_SIZE: u32 = 512;
const INODES_PER_CHUNK: u32 = 64;
/// The inode cluster: 8 KiB of 256-byte inodes, scaled with the inode
/// size.
const INODE_CLUSTER_BYTES: u32 = 8192 * INODE_SIZE / 256;
/// The five btree roots every group starts with.
const ROOT_BLOCKS: u32 = 5;
/// The free list the standard formatter leaves in each group: the
/// minimum the allocator keeps on hand for two one-level free-space
/// btrees to split.
const AGFL_FILL: u32 = 4;
/// The log the standard formatter never goes below, and the kernel
/// expects of any filesystem this size.
const LOG_MIN_BYTES: u64 = 64 * MIB;
/// The largest log the kernel accepts.
const LOG_MAX_BYTES: u64 = 2 * GIB - 1;
/// One log block per 2048 filesystem bytes, above the minimum.
const LOG_RATIO: u64 = 2048;
/// The in-core log buffer size an initialised log's record declares.
const LOG_BUFFER_BYTES: u32 = 32 * 1024;
/// Allocation group bounds.
const AG_MIN_BYTES: u64 = 16 * MIB;
const AG_MAX_BYTES: u64 = TIB;

/// What the caller can choose. Everything else is the standard
/// formatter's default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Filesystem block size in bytes.
    pub block_size: u32,
    /// Number of allocation groups, or `None` to choose from the size.
    pub agcount: Option<u32>,
    /// Volume label, at most [`MAX_LABEL_BYTES`].
    pub label: Option<String>,
    /// Volume UUID, or `None` for a random one.
    pub uuid: Option<[u8; 16]>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            block_size: DEFAULT_BLOCK_SIZE,
            agcount: None,
            label: None,
            uuid: None,
        }
    }
}

/// Where everything goes. Computed from the device size and the options
/// alone, before anything is written.
#[derive(Debug, Clone)]
pub struct Plan {
    sb: Superblock,
    /// Blocks in each group; only the last may be shorter.
    lengths: Vec<u32>,
    /// The group holding the log, and the block it starts at.
    log_ag: u32,
    log_agbno: u32,
    /// The first block of group 0's inode chunk.
    chunk_agbno: u32,
}

impl Plan {
    pub fn block_size(&self) -> u32 {
        self.sb.blocksize
    }
    pub fn agcount(&self) -> u32 {
        self.sb.agcount
    }
    pub fn agblocks(&self) -> u32 {
        self.sb.agblocks
    }
    pub fn logblocks(&self) -> u32 {
        self.sb.logblocks
    }
    /// The superblock that will be written.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    fn header_blocks(&self) -> u32 {
        (4 * SECTOR).div_ceil(self.sb.blocksize)
    }

    fn chunk_blocks(&self) -> u32 {
        INODES_PER_CHUNK * INODE_SIZE / self.sb.blocksize
    }

    /// The first block of group `ag`'s free list.
    fn agfl_start(&self, ag: u32) -> u32 {
        let after_roots = self.header_blocks() + ROOT_BLOCKS;
        if ag == self.log_ag {
            after_roots + self.sb.logblocks
        } else {
            after_roots
        }
    }

    /// Group `ag`'s free extents, `(start, length)` in block order.
    fn free_extents(&self, ag: u32) -> Vec<(u32, u32)> {
        let start = self.agfl_start(ag) + AGFL_FILL;
        let end = self.lengths[ag as usize];
        if ag != 0 {
            return vec![(start, end - start)];
        }
        let chunk_end = self.chunk_agbno + self.chunk_blocks();
        let mut out = Vec::new();
        if self.chunk_agbno > start {
            out.push((start, self.chunk_agbno - start));
        }
        if end > chunk_end {
            out.push((chunk_end, end - chunk_end));
        }
        out
    }

    fn free_blocks(&self, ag: u32) -> u32 {
        self.free_extents(ag).iter().map(|e| e.1).sum()
    }

    /// The 512-byte sector a block of group `ag` starts at.
    fn daddr(&self, ag: u32, agbno: u32) -> u64 {
        (u64::from(ag) * u64::from(self.sb.agblocks) + u64::from(agbno))
            * u64::from(self.sb.blocksize / SECTOR)
    }

    fn offset(&self, ag: u32, agbno: u32) -> u64 {
        self.daddr(ag, agbno) * u64::from(SECTOR)
    }
}

/// Work out the geometry for a device of `device_bytes`.
///
/// # Errors
///
/// [`Error::InvalidGeometry`] for a device too small, a block size or
/// group count out of range, or a label too long.
pub fn plan(device_bytes: u64, opts: &Options) -> Result<Plan> {
    let bs = opts.block_size;
    if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&bs) || !bs.is_power_of_two() {
        return Err(Error::InvalidGeometry(format!(
            "block size {bs} is not one this formatter makes: a power of two from \
             {MIN_BLOCK_SIZE} to {MAX_BLOCK_SIZE}"
        )));
    }
    if let Some(label) = &opts.label {
        if label.len() > MAX_LABEL_BYTES {
            return Err(Error::InvalidGeometry(format!(
                "label {label:?} is {} bytes; an XFS label holds {MAX_LABEL_BYTES}",
                label.len()
            )));
        }
    }
    if device_bytes < MIN_DEVICE_BYTES {
        return Err(Error::InvalidGeometry(format!(
            "the device is too small: {device_bytes} bytes, and an XFS filesystem needs \
             at least {MIN_DEVICE_BYTES} ({} MiB)",
            MIN_DEVICE_BYTES / MIB
        )));
    }
    let blocklog = bs.trailing_zeros() as u8;
    let dblocks = device_bytes / u64::from(bs);
    let ag_min = AG_MIN_BYTES / u64::from(bs);
    let ag_max = AG_MAX_BYTES / u64::from(bs);

    // Four groups up to 4 TiB, as the standard formatter does for one
    // device; above that, groups of the largest size.
    let agcount = match opts.agcount {
        Some(0) => {
            return Err(Error::InvalidGeometry(
                "agcount=0: a filesystem has at least one allocation group".into(),
            ))
        }
        Some(n) => u64::from(n),
        None if dblocks * u64::from(bs) >= 4 * TIB => dblocks.div_ceil(ag_max),
        None => 4,
    };
    let agblocks = dblocks.div_ceil(agcount);
    if agblocks > ag_max {
        return Err(Error::InvalidGeometry(format!(
            "agcount={agcount} makes groups of {agblocks} blocks; the largest is {ag_max}"
        )));
    }
    if agblocks < ag_min {
        return Err(Error::InvalidGeometry(format!(
            "agcount={agcount} makes groups of {agblocks} blocks; the smallest is {ag_min}"
        )));
    }
    let agcount = dblocks.div_ceil(agblocks);
    let last = dblocks - (agcount - 1) * agblocks;
    if last < ag_min {
        return Err(Error::InvalidGeometry(format!(
            "the last allocation group would be {last} blocks; the smallest is {ag_min}. \
             Choose an agcount that divides the device more evenly"
        )));
    }
    let agcount = u32::try_from(agcount)
        .map_err(|_| Error::InvalidGeometry(format!("{agcount} allocation groups")))?;
    let agblocks = agblocks as u32;
    let mut lengths = vec![agblocks; agcount as usize];
    lengths[agcount as usize - 1] = last as u32;

    // The log: one block per 2048 bytes of filesystem, never under 64 MiB
    // and never over what the kernel accepts, in the middle group.
    let log_bytes = (dblocks * u64::from(bs) / LOG_RATIO).clamp(LOG_MIN_BYTES, LOG_MAX_BYTES);
    let logblocks = (log_bytes / u64::from(bs)) as u32;
    let log_ag = agcount / 2;
    let header_blocks = (4 * SECTOR).div_ceil(bs);
    let log_agbno = header_blocks + ROOT_BLOCKS;
    if log_agbno + logblocks + AGFL_FILL >= lengths[log_ag as usize] {
        return Err(Error::InvalidGeometry(format!(
            "a {logblocks}-block log does not fit in an allocation group of {} blocks. \
             Ask for fewer, larger groups (-d agcount=)",
            lengths[log_ag as usize]
        )));
    }

    let inopblock = bs / INODE_SIZE;
    let inopblog = inopblock.trailing_zeros() as u8;
    let inoalignmt = INODES_PER_CHUNK * INODE_SIZE / bs;
    let spino_align = INODE_CLUSTER_BYTES / bs;
    let agblklog = 32 - (agblocks - 1).leading_zeros();

    // The first inode chunk, where xfs_repair will look for it.
    let mut first = header_blocks + ROOT_BLOCKS + AGFL_FILL;
    if log_ag == 0 {
        first += logblocks;
    }
    let chunk_agbno = first.div_ceil(inoalignmt) * inoalignmt;
    let rootino = u64::from(chunk_agbno) << inopblog;

    let uuid = opts.uuid.unwrap_or_else(random_uuid);
    let dirblksize = bs.max(4096);

    let mut sb = Superblock {
        blocksize: bs,
        dblocks,
        rblocks: 0,
        uuid,
        logstart: (u64::from(log_ag) << agblklog) | u64::from(log_agbno),
        rootino,
        agblocks,
        agcount,
        logblocks,
        // 0xb4a5, as measured: v5 with the inode-link, alignment, v2-log,
        // unwritten-extent, v2-directory and more-bits flags.
        versionnum: 5
            | version_flags::NLINKBIT
            | version_flags::ALIGNBIT
            | version_flags::LOGV2BIT
            | EXTFLGBIT
            | DIRV2BIT
            | version_flags::MOREBITSBIT,
        sectsize: SECTOR as u16,
        inodesize: INODE_SIZE as u16,
        inopblock: inopblock as u16,
        blocklog,
        sectlog: 9,
        inodelog: INODE_SIZE.trailing_zeros() as u8,
        inopblog,
        agblklog: agblklog as u8,
        inprogress: 0,
        icount: u64::from(INODES_PER_CHUNK),
        ifree: u64::from(INODES_PER_CHUNK - 3),
        fdblocks: 0,
        inoalignmt,
        dirblklog: (dirblksize / bs).trailing_zeros() as u8,
        logsunit: 1,
        // 0x18a, as measured. The directory file-type flag is the v5
        // incompat bit, not this one.
        features2: features2_flags::LAZYSBCOUNT
            | features2_flags::ATTR2
            | features2_flags::PROJID32BIT
            | CRCBIT,
        features_compat: 0,
        features_ro_compat: ro_compat::FINOBT | ro_compat::REFLINK | ro_compat::INOBTCNT,
        features_incompat: incompat::FTYPE | incompat::SPINODES | incompat::BIGTIME,
        features_log_incompat: 0,
        spino_align,
        meta_uuid: uuid,
        fname: opts.label.clone().unwrap_or_default(),
        rextents: 0,
        rbmino: rootino + 1,
        rsumino: rootino + 2,
        rextsize: (4096 / bs).max(1),
        rbmblocks: 0,
        rextslog: 0,
        imax_pct: imax_pct(dblocks * u64::from(bs)),
        frextents: 0,
        uquotino: 0,
        gquotino: 0,
        qflags: 0,
        flags: 0,
        shared_vn: 0,
        unit: 0,
        width: 0,
        logsectlog: 0,
        logsectsize: 0,
        bad_features2: 0,
        pquotino: 0,
        lsn: 0,
    };
    sb.bad_features2 = sb.features2;

    let mut plan = Plan {
        sb,
        lengths,
        log_ag,
        log_agbno,
        chunk_agbno,
    };
    if plan.chunk_agbno + plan.chunk_blocks() > plan.lengths[0] {
        return Err(Error::InvalidGeometry(
            "the first inode chunk does not fit in allocation group 0".into(),
        ));
    }
    // The free list's blocks count as free: they are the allocator's.
    plan.sb.fdblocks = (0..agcount)
        .map(|ag| u64::from(plan.free_blocks(ag) + AGFL_FILL))
        .sum();
    Ok(plan)
}

/// The share of the filesystem inodes may take, by size, as the standard
/// formatter chooses it.
fn imax_pct(bytes: u64) -> u8 {
    if bytes < TIB {
        25
    } else if bytes < 50 * TIB {
        5
    } else {
        1
    }
}

/// Make the filesystem `plan` describes on `dev`.
///
/// The primary superblock is written last, so a format interrupted
/// part-way leaves a device that does not look like a finished XFS
/// filesystem.
///
/// # Errors
///
/// [`Error::Io`] if the device refuses a write.
pub fn write(dev: &dyn BlockDevice, plan: &Plan) -> Result<()> {
    let size = dev.size_bytes();

    // Old signatures go first: the first and last MiB of the device, so a
    // previous filesystem's superblock copies, or another filesystem's
    // magic, cannot outlive this one.
    zero(dev, 0, MIB.min(size))?;
    zero(dev, size.saturating_sub(MIB), MIB.min(size))?;

    for ag in 0..plan.sb.agcount {
        write_group(dev, plan, ag)?;
    }

    write_inode_chunk(dev, plan)?;
    write_log(dev, plan)?;

    // Block 0 also holds the AGF, AGI and AGFL, written by write_group:
    // only the superblock's own sector is written here.
    let mut sector = vec![0u8; SECTOR as usize];
    crate::super_write::apply(&mut sector, &plan.sb)?;
    dev.write_at(0, &sector)?;
    dev.flush()?;
    Ok(())
}

/// Plan and write in one call. Returns the superblock written.
pub fn format(dev: &dyn BlockDevice, opts: &Options) -> Result<Superblock> {
    let plan = plan(dev.size_bytes(), opts)?;
    write(dev, &plan)?;
    Ok(plan.sb)
}

/// What `dev` already holds that a format would destroy, if it is
/// something recognisable: the check `mkfs.xfs` makes before it will
/// overwrite a device without `-f`.
pub fn existing_signature(dev: &dyn BlockRead) -> Option<&'static str> {
    let mut head = vec![0u8; 128 * 1024];
    let n = (dev.size_bytes() as usize).min(head.len());
    dev.read_at(0, &mut head[..n]).ok()?;
    let head = &head[..n];
    let at = |off: usize, magic: &[u8]| head.get(off..off + magic.len()) == Some(magic);
    if at(0, b"XFSB") {
        Some("an XFS filesystem")
    } else if at(1080, &[0x53, 0xef]) {
        Some("an ext2/3/4 filesystem")
    } else if at(3, b"NTFS    ") {
        Some("an NTFS filesystem")
    } else if at(65536 + 64, b"_BHRfS_M") {
        Some("a Btrfs filesystem")
    } else if at(1024, &[0xe2, 0xe1, 0xf5, 0xe0]) {
        Some("an EROFS filesystem")
    } else if at(0, b"hsqs") {
        Some("a SquashFS filesystem")
    } else if at(3, b"MSDOS5.0") || at(82, b"FAT32   ") || at(54, b"FAT1") {
        Some("a FAT filesystem")
    } else if at(3, b"EXFAT   ") {
        Some("an exFAT filesystem")
    } else if at(512, b"EFI PART") {
        Some("a GPT partition table")
    } else if at(510, &[0x55, 0xaa]) && head[446..510].iter().any(|b| *b != 0) {
        Some("an MBR partition table")
    } else {
        None
    }
}

fn zero(dev: &dyn BlockDevice, offset: u64, len: u64) -> Result<()> {
    const CHUNK: u64 = 4 * MIB;
    let buf = vec![0u8; CHUNK.min(len) as usize];
    let mut done = 0;
    while done < len {
        let n = CHUNK.min(len - done);
        dev.write_at(offset + done, &buf[..n as usize])?;
        done += n;
    }
    Ok(())
}

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_be_bytes());
}
fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}
fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_be_bytes());
}
/// CRC32C over `buf` with the field at `at` zeroed, stored little-endian
/// there: every v5 checksum is the same shape.
fn stamp(buf: &mut [u8], at: usize) {
    let crc = crc32c_with_zeroed_crc(buf, at);
    buf[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

/// One group's header block and its five btree roots.
fn write_group(dev: &dyn BlockDevice, plan: &Plan, ag: u32) -> Result<()> {
    let sb = &plan.sb;
    let bs = sb.blocksize as usize;
    let sect = SECTOR as usize;
    let header_bytes = plan.header_blocks() as usize * bs;
    let mut header = vec![0u8; header_bytes];
    let length = plan.lengths[ag as usize];
    let hb = plan.header_blocks();
    let (bno_root, cnt_root, ino_root, fino_root, refc_root) = (hb, hb + 1, hb + 2, hb + 3, hb + 4);
    let free = plan.free_extents(ag);

    // The superblock copy. Group 0's is the primary, written last by
    // `write`; the secondaries are what the standard formatter leaves in
    // them, which is the primary as it stood before the inode chunk was
    // allocated and while the format was still in progress: the realtime
    // inodes not yet made (NULLFSINO), and in the log's group the root
    // inode not yet chosen either.
    if ag != 0 {
        let mut copy = sb.clone();
        copy.inprogress = 1;
        copy.rbmino = u64::MAX;
        copy.rsumino = u64::MAX;
        if ag == plan.log_ag {
            copy.rootino = u64::MAX;
        }
        copy.icount = 0;
        copy.ifree = 0;
        copy.fdblocks = sb.fdblocks + u64::from(plan.chunk_blocks());
        crate::super_write::apply(&mut header[..sect], &copy)?;
    }

    // AGF.
    {
        let agf = &mut header[sect..2 * sect];
        put32(agf, ago::common::MAGIC, XFS_AGF_MAGIC);
        put32(agf, ago::common::VERSIONNUM, 1);
        put32(agf, ago::common::SEQNO, ag);
        put32(agf, ago::common::LENGTH, length);
        put32(agf, ago::agf::ROOTS, bno_root);
        put32(agf, ago::agf::ROOTS + 4, cnt_root);
        put32(agf, ago::agf::LEVELS, 1);
        put32(agf, ago::agf::LEVELS + 4, 1);
        put32(agf, ago::agf::FLFIRST, 1);
        put32(agf, ago::agf::FLLAST, AGFL_FILL);
        put32(agf, ago::agf::FLCOUNT, AGFL_FILL);
        put32(agf, ago::agf::FREEBLKS, plan.free_blocks(ag));
        put32(
            agf,
            ago::agf::LONGEST,
            free.iter().map(|e| e.1).max().unwrap_or(0),
        );
        agf[ago::agf::UUID..ago::agf::UUID + 16].copy_from_slice(&sb.meta_uuid);
        put32(agf, ago::agf::REFCOUNT_BLOCKS, 1);
        put32(agf, ago::agf::REFCOUNT_ROOT, refc_root);
        put32(agf, ago::agf::REFCOUNT_LEVEL, 1);
        stamp(agf, ago::agf::CRC);
    }

    // AGI.
    {
        let agi = &mut header[2 * sect..3 * sect];
        put32(agi, ago::common::MAGIC, XFS_AGI_MAGIC);
        put32(agi, ago::common::VERSIONNUM, 1);
        put32(agi, ago::common::SEQNO, ag);
        put32(agi, ago::common::LENGTH, length);
        let (count, freecount, newino) = if ag == 0 {
            (
                INODES_PER_CHUNK,
                INODES_PER_CHUNK - 3,
                plan.chunk_agbno << sb.inopblog,
            )
        } else {
            (0, 0, u32::MAX)
        };
        put32(agi, ago::agi::COUNT, count);
        put32(agi, ago::agi::ROOT, ino_root);
        put32(agi, ago::agi::LEVEL, 1);
        put32(agi, ago::agi::FREECOUNT, freecount);
        put32(agi, ago::agi::NEWINO, newino);
        put32(agi, ago::agi::DIRINO, u32::MAX);
        for bucket in 0..crate::ag::XFS_AGI_UNLINKED_BUCKETS {
            put32(agi, ago::agi::UNLINKED + 4 * bucket, u32::MAX);
        }
        agi[ago::agi::UUID..ago::agi::UUID + 16].copy_from_slice(&sb.meta_uuid);
        put32(agi, ago::agi::FREE_ROOT, fino_root);
        put32(agi, ago::agi::FREE_LEVEL, 1);
        // agi_iblocks and agi_fblocks: one block each for the two roots.
        put32(agi, ago::agi::FREE_LEVEL + 4, 1);
        put32(agi, ago::agi::FREE_LEVEL + 8, 1);
        stamp(agi, ago::agi::CRC);
    }

    // AGFL: four blocks in slots 1..=4, every other slot empty.
    {
        let agfl = &mut header[3 * sect..4 * sect];
        put32(agfl, crate::agfl::offsets::MAGIC, XFS_AGFL_MAGIC);
        put32(agfl, crate::agfl::offsets::SEQNO, ag);
        agfl[crate::agfl::offsets::UUID..crate::agfl::offsets::UUID + 16]
            .copy_from_slice(&sb.meta_uuid);
        let first = crate::agfl::offsets::CRC + 4;
        let slots = (sect - first) / crate::agfl::ENTRY_LEN;
        for slot in 0..slots {
            put32(agfl, first + 4 * slot, NULLAGBLOCK);
        }
        let start = plan.agfl_start(ag);
        for i in 0..AGFL_FILL {
            put32(agfl, first + 4 * (1 + i as usize), start + i);
        }
        stamp(agfl, crate::agfl::offsets::CRC);
    }
    dev.write_at(plan.offset(ag, 0), &header)?;

    // The five roots, each a leaf.
    let by_count = {
        let mut v = free.clone();
        v.sort_by_key(|&(start, len)| (len, start));
        v
    };
    let leaf = |magic: u32, agbno: u32, records: &[Vec<u8>]| -> Vec<u8> {
        let mut b = vec![0u8; bs];
        put32(&mut b, bto::MAGIC, magic);
        put16(&mut b, bto::NUMRECS, records.len() as u16);
        put32(&mut b, bto::NUMRECS + 2, NULLAGBLOCK);
        put32(&mut b, bto::NUMRECS + 6, NULLAGBLOCK);
        put64(&mut b, bto::BLKNO, plan.daddr(ag, agbno));
        b[bto::UUID..bto::UUID + 16].copy_from_slice(&sb.meta_uuid);
        put32(&mut b, bto::OWNER, ag);
        let mut at = 56;
        for r in records {
            b[at..at + r.len()].copy_from_slice(r);
            at += r.len();
        }
        stamp(&mut b, bto::CRC);
        b
    };
    let extent = |&(start, len): &(u32, u32)| {
        let mut r = vec![0u8; 8];
        put32(&mut r, 0, start);
        put32(&mut r, 4, len);
        r
    };
    let chunk: Vec<Vec<u8>> = if ag == 0 {
        let mut r = vec![0u8; 16];
        put32(&mut r, 0, plan.chunk_agbno << sb.inopblog);
        // holemask 0 (no holes), 64 inodes, 61 free: all but the first three.
        r[6] = INODES_PER_CHUNK as u8;
        r[7] = (INODES_PER_CHUNK - 3) as u8;
        put64(&mut r, 8, !0b111u64);
        vec![r]
    } else {
        Vec::new()
    };
    let roots = [
        (
            bno_root,
            XFS_ABTB_CRC_MAGIC,
            free.iter().map(extent).collect::<Vec<_>>(),
        ),
        (
            cnt_root,
            XFS_ABTC_CRC_MAGIC,
            by_count.iter().map(extent).collect(),
        ),
        (ino_root, XFS_IBT_CRC_MAGIC, chunk.clone()),
        (fino_root, XFS_FIBT_CRC_MAGIC, chunk),
        (refc_root, XFS_REFC_CRC_MAGIC, Vec::new()),
    ];
    let mut blocks = Vec::with_capacity(ROOT_BLOCKS as usize * bs);
    for (agbno, magic, records) in &roots {
        blocks.extend_from_slice(&leaf(*magic, *agbno, records));
    }
    dev.write_at(plan.offset(ag, bno_root), &blocks)?;
    Ok(())
}

/// Nanoseconds since the large-timestamp epoch (1901-12-13T20:45:52Z).
fn bigtime(secs: i64, nsec: u32) -> u64 {
    ((secs + (1i64 << 31)) as u64) * 1_000_000_000 + u64::from(nsec)
}

/// Group 0's inode chunk: the root directory, the realtime bitmap and
/// summary inodes, and 61 free inodes.
fn write_inode_chunk(dev: &dyn BlockDevice, plan: &Plan) -> Result<()> {
    let sb = &plan.sb;
    let isz = INODE_SIZE as usize;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| bigtime(d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or_else(|_| bigtime(0, 0));
    let epoch = bigtime(0, 0);
    let mut chunk = vec![0u8; INODES_PER_CHUNK as usize * isz];
    for n in 0..INODES_PER_CHUNK as usize {
        let ino = sb.rootino + n as u64;
        let d = &mut chunk[n * isz..(n + 1) * isz];
        put16(d, io::MAGIC, crate::inode::XFS_DINODE_MAGIC);
        d[io::VERSION] = 3;
        put32(d, io::NEXT_UNLINKED, u32::MAX);
        put64(d, io::INO, ino);
        d[io::UUID..io::UUID + 16].copy_from_slice(&sb.meta_uuid);
        if n < 3 {
            // The three inodes the format allocates.
            let (mode, format, nlink, size, flags) = match n {
                // The root directory: an empty short-form directory,
                // whose parent is itself.
                0 => (0o040755u16, 1u8, 2u32, 6u64, 0u16),
                // The realtime bitmap, flagged as using the new layout.
                1 => (0o100000, 2, 1, 0, 0x0004),
                // The realtime summary.
                _ => (0o100000, 2, 1, 0, 0),
            };
            put16(d, io::MODE, mode);
            d[io::FORMAT] = format;
            put32(d, io::NLINK, nlink);
            put64(d, io::ATIME, epoch);
            put64(d, io::MTIME, now);
            put64(d, io::CTIME, now);
            put64(d, io::SIZE, size);
            d[io::AFORMAT] = 2;
            put16(d, io::FLAGS, flags);
            put64(d, io::CHANGECOUNT, 2);
            put64(d, io::FLAGS2, 0x8); // XFS_DIFLAG2_BIGTIME
            put64(d, io::CRTIME, now);
            if n == 0 {
                // sf header: count 0, i8count 0, parent (4 bytes).
                let fork = crate::inode::XFS_DINODE_V3_SIZE;
                put32(d, fork + 2, sb.rootino as u32);
            }
        }
        stamp(d, io::CRC);
    }
    dev.write_at(plan.offset(0, plan.chunk_agbno), &chunk)
        .map_err(Error::from)
}

/// The log: zeroed, then one record at its head holding an unmount, as a
/// cleanly unmounted log at cycle 1 does.
fn write_log(dev: &dyn BlockDevice, plan: &Plan) -> Result<()> {
    let sb = &plan.sb;
    let start = plan.offset(plan.log_ag, plan.log_agbno);
    zero(
        dev,
        start,
        u64::from(sb.logblocks) * u64::from(sb.blocksize),
    )?;

    let mut rec = vec![0u8; 2 * SECTOR as usize];
    let lsn: u64 = 1 << 32; // cycle 1, block 0
    put32(&mut rec, 0, 0xfeed_babe); // h_magicno
    put32(&mut rec, 4, 1); // h_cycle
    put32(&mut rec, 8, 2); // h_version: v2 log
    put32(&mut rec, 12, SECTOR); // h_len: one basic block of data
    put64(&mut rec, 16, lsn); // h_lsn
    put64(&mut rec, 24, lsn); // h_tail_lsn
    put32(&mut rec, 36, u32::MAX); // h_prev_block: none
    put32(&mut rec, 40, 1); // h_num_logops
                            // h_cycle_data[0]: the word the cycle number displaced from the data
                            // block below, which is the unmount op's transaction id.
    put32(&mut rec, 44, 0xb0c0_d0d0);
    put32(&mut rec, 300, 1); // h_fmt: XLOG_FMT_LINUX_LE
    rec[304..320].copy_from_slice(&sb.uuid); // h_fs_uuid
    put32(&mut rec, 320, LOG_BUFFER_BYTES); // h_size

    // The data block: the unmount op, its first word replaced by the
    // cycle number as every log block's is.
    let data = &mut rec[SECTOR as usize..];
    put32(data, 0, 1); // cycle stamp (was oh_tid 0xb0c0d0d0)
    put32(data, 4, 8); // oh_len: the unmount record
    data[8] = 0xaa; // oh_clientid: XFS_LOG
    data[9] = 0x20; // oh_flags: XLOG_UNMOUNT_TRANS
                    // The unmount record, in the writer's (little-endian) order.
    data[12..14].copy_from_slice(&0x556eu16.to_le_bytes());
    dev.write_at(start, &rec).map_err(Error::from)
}

/// A random version-4 UUID, from the operating system's generator where
/// there is one.
fn random_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut u))
        .is_ok();
    if !filled {
        // No /dev/urandom (Windows): the standard library's per-process
        // random hash keys, mixed with the clock.
        use std::hash::{BuildHasher, Hasher};
        for half in 0..2 {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            h.write_usize(half);
            u[half * 8..half * 8 + 8].copy_from_slice(&h.finish().to_be_bytes());
        }
    }
    u[6] = (u[6] & 0x0f) | 0x40;
    u[8] = (u[8] & 0x3f) | 0x80;
    u
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB400: u64 = 400 * MIB;

    /// The geometry the standard formatter chose for the 400 MiB fixtures,
    /// at each block size, is the geometry planned here.
    #[test]
    fn the_plan_matches_the_measured_fixtures() {
        for (bs, agblocks, logstart, logblocks, rootino, inoalignmt, spino, dirblklog, rextsize) in [
            (
                4096u32, 25600u32, 65542u64, 16384u32, 128u64, 8u32, 4u32, 0u8, 1u32,
            ),
            (2048, 51200, 131078, 32768, 64, 16, 8, 1, 2),
            (1024, 102400, 262151, 65536, 64, 32, 16, 2, 4),
        ] {
            let p = plan(
                MIB400,
                &Options {
                    block_size: bs,
                    ..Options::default()
                },
            )
            .unwrap();
            let sb = p.superblock();
            assert_eq!(sb.agcount, 4, "{bs}");
            assert_eq!(sb.agblocks, agblocks, "{bs}");
            assert_eq!(sb.logstart, logstart, "{bs}");
            assert_eq!(sb.logblocks, logblocks, "{bs}");
            assert_eq!(sb.rootino, rootino, "{bs}");
            assert_eq!(sb.inoalignmt, inoalignmt, "{bs}");
            assert_eq!(sb.spino_align, spino, "{bs}");
            assert_eq!(sb.dirblklog, dirblklog, "{bs}");
            assert_eq!(sb.rextsize, rextsize, "{bs}");
            assert_eq!(sb.versionnum, 0xb4a5, "{bs}");
            assert_eq!(sb.features2, 0x18a, "{bs}");
            assert_eq!(sb.features_ro_compat, 0xd, "{bs}");
            assert_eq!(sb.features_incompat, 0xb, "{bs}");
        }
        // And the free-block count the 4 KiB fixture's superblock holds.
        let p = plan(MIB400, &Options::default()).unwrap();
        assert_eq!(p.superblock().fdblocks, 85984);
        assert_eq!(p.free_extents(0), vec![(10, 6), (24, 25576)]);
        assert_eq!(p.free_extents(2), vec![(16394, 9206)]);
    }

    #[test]
    fn a_device_under_300_mib_is_too_small() {
        let e = plan(299 * MIB, &Options::default()).unwrap_err();
        assert!(e.to_string().contains("too small"), "{e}");
    }

    #[test]
    fn a_block_size_outside_the_tested_range_is_refused() {
        for bs in [512u32, 3000, 8192, 65536] {
            let opts = Options {
                block_size: bs,
                ..Options::default()
            };
            assert!(plan(GIB, &opts).is_err(), "{bs}");
        }
    }

    #[test]
    fn a_thirteen_byte_label_is_refused() {
        let opts = Options {
            label: Some("ABCDEFGHIJKLM".into()),
            ..Options::default()
        };
        assert!(plan(GIB, &opts).is_err());
    }
}
