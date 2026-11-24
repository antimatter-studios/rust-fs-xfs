//! Superblocks, allocation group headers and the log, read for salvage.

use super::evidence::{finding, Damage, DamageKind, Extent, Integrity, Severity};
use crate::ag::XFS_AGI_SIZE;
use crate::ag::{self, Agf, Agi, XFS_AGFL_MAGIC, XFS_AGF_MAGIC, XFS_AGF_SIZE, XFS_AGI_MAGIC};
use crate::agfl;
use crate::endian::{be16, be32, le32, uuid_at};
use crate::log::{self, LogState};
use crate::superblock::{
    crc32c_with_zeroed_crc, incompat, offsets as sb_offsets, Superblock, XFS_SB_MAGIC,
};
use fs_core::BlockRead;

/// The smallest sector XFS allows, and the unit superblocks are aligned to.
const MIN_SECTOR: u64 = 512;

/// The largest sector XFS allows: the most a superblock read can need.
const MAX_SECTOR: u16 = 32768;

/// Bytes a superblock scan reads at a time.
const SCAN_CHUNK: u64 = 1 << 20;

/// One superblock, primary or copy, as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuperblockCopy {
    /// The allocation group this copy heads: 0 for the primary. `None` for
    /// a superblock a scan found at an offset no group starts at under its
    /// own geometry.
    pub ag: Option<u32>,
    /// The sector holding it.
    pub location: Extent,
    pub integrity: Integrity,
    /// The fields, read past a failed checksum, an unfinished format or the
    /// needs-repair flag. `None` when the magic is wrong, the sector could
    /// not be read, or the geometry is impossible.
    pub superblock: Option<Superblock>,
    /// What is wrong with this copy on its own. Disagreements with the
    /// chosen geometry are in [`Superblocks::damage`].
    pub damage: Vec<Damage>,
}

/// Every superblock of a volume, and the geometry chosen from them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Superblocks {
    /// The geometry the rest of the volume is read with: the primary's,
    /// unless it is unusable or most copies disagree with it, and then the
    /// one most copies share. `None` when no superblock could be decoded.
    pub geometry: Option<Superblock>,
    /// The allocation group whose superblock `geometry` was taken from.
    pub source: Option<u32>,
    /// The primary first, then each copy in group order.
    pub copies: Vec<SuperblockCopy>,
    /// Copies that disagree with `geometry`, or with its UUID.
    pub damage: Vec<Damage>,
}

impl Superblocks {
    /// Every finding: each copy's own, then the disagreements.
    pub fn all_damage(&self) -> impl Iterator<Item = &Damage> {
        self.copies
            .iter()
            .flat_map(|copy| copy.damage.iter())
            .chain(self.damage.iter())
    }
}

/// Read and check one superblock sector at `offset`.
///
/// `ag` is the group the sector should head, which decides only how
/// severe its damage is: the primary is what a mount reads, and a copy
/// is covered by the others.
pub fn read_superblock(device: &dyn BlockRead, ag: Option<u32>, offset: u64) -> SuperblockCopy {
    let structure = match ag {
        Some(ag) => format!("superblock {ag}"),
        None => format!("superblock at byte {offset}"),
    };
    let severity = if ag == Some(0) {
        Severity::Error
    } else {
        Severity::Warning
    };
    let mut copy = SuperblockCopy {
        ag,
        location: Extent::new(offset, MIN_SECTOR),
        integrity: Integrity::Unchecked,
        superblock: None,
        damage: Vec::new(),
    };
    let mut buf = vec![0u8; MIN_SECTOR as usize];
    if let Err(e) = device.read_at(offset, &mut buf) {
        copy.damage.push(finding(
            DamageKind::Unreadable,
            severity,
            copy.location,
            structure,
            e.to_string(),
        ));
        return copy;
    }
    decode_superblock(device, &mut copy, buf, &structure, severity);
    copy
}

/// The checks and the decode of a superblock whose first sector is in
/// `buf`, reading the rest of a larger sector when it declares one.
fn decode_superblock(
    device: &dyn BlockRead,
    copy: &mut SuperblockCopy,
    mut buf: Vec<u8>,
    structure: &str,
    severity: Severity,
) {
    let magic = be32(&buf, sb_offsets::MAGIC);
    if magic != XFS_SB_MAGIC {
        copy.damage.push(finding(
            DamageKind::Signature,
            severity,
            copy.location,
            structure,
            format!("magic {magic:#010x}, expected {XFS_SB_MAGIC:#010x}"),
        ));
        return;
    }
    let sectsize = be16(&buf, sb_offsets::SECTSIZE);
    if (MIN_SECTOR as u16..=MAX_SECTOR).contains(&sectsize)
        && sectsize.is_power_of_two()
        && usize::from(sectsize) > buf.len()
    {
        let mut whole = vec![0u8; usize::from(sectsize)];
        match device.read_at(copy.location.offset, &mut whole) {
            Ok(()) => {
                buf = whole;
                copy.location.length = u64::from(sectsize);
            }
            Err(e) => copy.damage.push(finding(
                DamageKind::Unreadable,
                severity,
                Extent::new(copy.location.offset, u64::from(sectsize)),
                structure,
                format!("the rest of the {sectsize}-byte sector: {e}"),
            )),
        }
    }
    copy.integrity = match Superblock::checksum_matches(&buf) {
        Some(true) => Integrity::Verified,
        Some(false) => {
            copy.damage.push(finding(
                DamageKind::Checksum,
                severity,
                copy.location,
                structure,
                format!(
                    "stored CRC {:#010x}, computed {:#010x}",
                    le32(&buf, sb_offsets::CRC),
                    crc32c_with_zeroed_crc(&buf, sb_offsets::CRC)
                ),
            ));
            Integrity::Failed
        }
        None => Integrity::Unchecked,
    };
    match Superblock::parse_lenient(&buf) {
        Ok(sb) => {
            if sb.is_v5() && sb.features_incompat & incompat::NEEDSREPAIR != 0 {
                copy.damage.push(finding(
                    DamageKind::Recorded,
                    severity,
                    copy.location,
                    structure,
                    "the needs-repair flag is set: a repair was started and did not finish",
                ));
            }
            // mkfs leaves sb_inprogress set in every copy but the primary,
            // so only the primary is damaged by it.
            if sb.inprogress != 0 && copy.ag == Some(0) {
                copy.damage.push(finding(
                    DamageKind::Recorded,
                    severity,
                    copy.location,
                    structure,
                    "sb_inprogress is set: the format did not finish",
                ));
            }
            copy.superblock = Some(sb);
        }
        Err(e) => copy.damage.push(finding(
            DamageKind::Malformed,
            severity,
            copy.location,
            structure,
            e.to_string(),
        )),
    }
}

/// The primary superblock, every copy, and the geometry chosen from them.
///
/// The copies are found from the primary's geometry. When the primary
/// cannot be decoded, the volume is scanned for the first copy that can
/// be ([`scan_superblocks`]) and the copies are found from its geometry
/// instead; on a large volume whose primary is gone that scan reads up to
/// the second allocation group.
pub fn superblocks(device: &dyn BlockRead) -> Superblocks {
    let primary = read_superblock(device, Some(0), 0);
    let mut copies = vec![primary];
    let first = match copies[0].superblock.clone() {
        Some(sb) => Some(sb),
        None => first_copy_by_scan(device),
    };
    let Some(first) = first else {
        return Superblocks {
            geometry: None,
            source: None,
            copies,
            damage: Vec::new(),
        };
    };
    for ag in 1..group_count(&first) {
        copies.push(read_superblock(device, Some(ag), first.ag_offset(ag)));
    }
    let (geometry, source) = choose_geometry(&copies);
    let mut damage = Vec::new();
    if let Some(geometry) = &geometry {
        for copy in &copies {
            let Some(sb) = &copy.superblock else { continue };
            let structure = format!("superblock {}", copy.ag.unwrap_or_default());
            let severity = if copy.ag == Some(0) {
                Severity::Error
            } else {
                Severity::Warning
            };
            for (name, chosen, theirs) in geometry.geometry_differences(sb) {
                damage.push(finding(
                    DamageKind::Mismatch,
                    severity,
                    copy.location,
                    structure.clone(),
                    format!("{name} is {theirs}; the volume's geometry says {chosen}"),
                ));
            }
            if sb.uuid != geometry.uuid {
                damage.push(finding(
                    DamageKind::Mismatch,
                    severity,
                    copy.location,
                    structure,
                    "the UUID is another volume's",
                ));
            }
        }
    }
    Superblocks {
        geometry,
        source,
        copies,
        damage,
    }
}

/// How many groups `sb` describes, bounded by what its own block count
/// allows so a damaged `agcount` cannot ask for millions of reads.
fn group_count(sb: &Superblock) -> u32 {
    if sb.agblocks == 0 {
        return 1;
    }
    let by_size = sb.dblocks.div_ceil(u64::from(sb.agblocks));
    sb.agcount
        .min(u32::try_from(by_size).unwrap_or(u32::MAX))
        .max(1)
}

/// The first superblock copy a scan finds that heads a group under its own
/// geometry, searching from the second sector.
fn first_copy_by_scan(device: &dyn BlockRead) -> Option<Superblock> {
    let mut found = None;
    let end = readable_end(device);
    scan_superblocks(device, MIN_SECTOR, end, &mut |copy: SuperblockCopy| {
        if copy.ag.is_some_and(|ag| ag > 0) {
            found = copy.superblock;
            false
        } else {
            true
        }
    });
    found
}

/// Where the device ends, found by reading: the first unreadable sector
/// at a power of two bounds it, and a binary search between that and the
/// last readable one finds it.
///
/// [`BlockRead`] has no size. On a device with unreadable sectors this can
/// stop early, at a bad sector that happens to fall on a probe; a scan
/// bounded by it then ends there, and says so by finding nothing past it.
fn readable_end(device: &dyn BlockRead) -> u64 {
    let readable = |sector: u64| {
        let mut one = [0u8; MIN_SECTOR as usize];
        sector
            .checked_mul(MIN_SECTOR)
            .is_some_and(|at| device.read_at(at, &mut one).is_ok())
    };
    if !readable(0) {
        return 0;
    }
    // Sectors `low` is readable and `high` is not.
    let mut low = 0u64;
    let mut high = 1u64;
    while readable(high) {
        low = high;
        match high.checked_mul(2) {
            Some(next) if next < u64::MAX / MIN_SECTOR => high = next,
            _ => return u64::MAX,
        }
    }
    while high - low > 1 {
        let middle = low + (high - low) / 2;
        if readable(middle) {
            low = middle;
        } else {
            high = middle;
        }
    }
    high * MIN_SECTOR
}

/// Pick the geometry: the primary's if it decoded, passed its checksum or
/// has at least as many copies agreeing with it as any other, and
/// otherwise the geometry most copies share, taken from the lowest group
/// that has it.
fn choose_geometry(copies: &[SuperblockCopy]) -> (Option<Superblock>, Option<u32>) {
    let decoded: Vec<(u32, &Superblock, Integrity)> = copies
        .iter()
        .filter_map(|c| Some((c.ag?, c.superblock.as_ref()?, c.integrity)))
        .collect();
    let agreeing = |sb: &Superblock| {
        decoded
            .iter()
            .filter(|(_, other, _)| {
                other.uuid == sb.uuid && sb.geometry_differences(other).is_empty()
            })
            .count()
    };
    let best = decoded
        .iter()
        .map(|(ag, sb, integrity)| {
            // More agreement wins; at a tie a verified checksum, then the
            // lower group, which puts the primary first.
            let verified = *integrity != Integrity::Failed;
            ((agreeing(sb), verified, std::cmp::Reverse(*ag)), *ag, *sb)
        })
        .max_by_key(|(rank, _, _)| *rank);
    match best {
        Some((_, ag, sb)) => (Some(sb.clone()), Some(ag)),
        None => (None, None),
    }
}

/// Scan `[from, to)` for superblocks by their magic, at every 512-byte
/// boundary, passing each one decoded to `found` until it returns `false`.
/// Returns the ranges that could not be read.
///
/// A superblock the scan finds says where it sits under its own geometry:
/// its [`SuperblockCopy::ag`] is the group it heads, or `None` when no
/// group starts at its offset, which is how a stray copy (an old volume's,
/// or one inside a file) shows itself.
pub fn scan_superblocks(
    device: &dyn BlockRead,
    from: u64,
    to: u64,
    found: &mut dyn FnMut(SuperblockCopy) -> bool,
) -> Vec<Extent> {
    let mut unreadable = Vec::new();
    let mut at = from - from % MIN_SECTOR;
    let mut chunk = vec![0u8; SCAN_CHUNK as usize];
    while at < to {
        let length = SCAN_CHUNK.min(to - at);
        let buf = &mut chunk[..length as usize];
        if device.read_at(at, buf).is_err() {
            // Past the end of the device, or a bad region: try it a sector
            // at a time, so one bad sector does not hide a megabyte.
            for sector in (0..length).step_by(MIN_SECTOR as usize) {
                let mut one = [0u8; MIN_SECTOR as usize];
                match device.read_at(at + sector, &mut one) {
                    Ok(()) => {
                        if !visit(device, at + sector, &one, found) {
                            return unreadable;
                        }
                    }
                    Err(_) => push_merged(&mut unreadable, Extent::new(at + sector, MIN_SECTOR)),
                }
            }
            at += length;
            continue;
        }
        for sector in (0..length).step_by(MIN_SECTOR as usize) {
            let start = sector as usize;
            let one = &buf[start..start + MIN_SECTOR as usize];
            if be32(one, sb_offsets::MAGIC) == XFS_SB_MAGIC
                && !visit(device, at + sector, one, found)
            {
                return unreadable;
            }
        }
        at += length;
    }
    unreadable
}

/// Decode the sector at `offset` if it holds a superblock's magic, and
/// pass it on. Returns whether to keep scanning.
fn visit(
    device: &dyn BlockRead,
    offset: u64,
    sector: &[u8],
    found: &mut dyn FnMut(SuperblockCopy) -> bool,
) -> bool {
    if be32(sector, sb_offsets::MAGIC) != XFS_SB_MAGIC {
        return true;
    }
    let mut copy = SuperblockCopy {
        ag: None,
        location: Extent::new(offset, MIN_SECTOR),
        integrity: Integrity::Unchecked,
        superblock: None,
        damage: Vec::new(),
    };
    let structure = format!("superblock at byte {offset}");
    decode_superblock(
        device,
        &mut copy,
        sector.to_vec(),
        &structure,
        Severity::Warning,
    );
    if let Some(sb) = &copy.superblock {
        let group = u64::from(sb.agblocks) * u64::from(sb.blocksize);
        if group != 0 && offset.is_multiple_of(group) {
            copy.ag = u32::try_from(offset / group).ok();
        }
    }
    found(copy)
}

fn push_merged(ranges: &mut Vec<Extent>, next: Extent) {
    if let Some(last) = ranges.last_mut() {
        if last.end() == Some(next.offset) {
            last.length += next.length;
            return;
        }
    }
    ranges.push(next);
}

/// One allocation group header as read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header<T> {
    /// The sector holding it.
    pub location: Extent,
    pub integrity: Integrity,
    /// The fields as stored. `None` when the sector could not be read or
    /// its magic is wrong, so that it is not this header at all.
    pub value: Option<T>,
    pub damage: Vec<Damage>,
}

/// The three headers of one allocation group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgHeaders {
    pub ag: u32,
    pub agf: Header<Agf>,
    pub agi: Header<Agi>,
    /// The free list's slots, every one as stored, in slot order.
    pub agfl: Header<Vec<u32>>,
}

impl AgHeaders {
    /// Every finding in the three headers.
    pub fn all_damage(&self) -> impl Iterator<Item = &Damage> {
        self.agf
            .damage
            .iter()
            .chain(self.agi.damage.iter())
            .chain(self.agfl.damage.iter())
    }
}

/// Where a v5 header keeps its checksum, UUID and group number.
struct Identity {
    crc: usize,
    uuid: usize,
    seqno: usize,
}

/// Read the AGF, AGI and AGFL of group `ag` under the geometry `sb`.
///
/// Each header is checked the way its strict parser checks it (magic,
/// v5 checksum, the volume's UUID and the group's number, the fields'
/// ranges) and every failure is a finding. The fields are decoded whatever
/// the checks found, once the magic says the sector is the header.
pub fn ag_headers(device: &dyn BlockRead, sb: &Superblock, ag: u32) -> AgHeaders {
    let start = sb.ag_offset(ag);
    let sector = u64::from(sb.sectsize);
    let agf = read_header(
        device,
        sb,
        ag,
        start + sector,
        "AGF",
        XFS_AGF_MAGIC,
        XFS_AGF_SIZE,
        Some(Identity {
            crc: ag::offsets::agf::CRC,
            uuid: ag::offsets::agf::UUID,
            seqno: ag::offsets::common::SEQNO,
        }),
        Agf::decode,
        agf_ranges,
    );
    let agi = read_header(
        device,
        sb,
        ag,
        start + 2 * sector,
        "AGI",
        XFS_AGI_MAGIC,
        XFS_AGI_SIZE,
        Some(Identity {
            crc: ag::offsets::agi::CRC,
            uuid: ag::offsets::agi::UUID,
            seqno: ag::offsets::common::SEQNO,
        }),
        Agi::decode,
        agi_ranges,
    );
    // A v4 free list has no header: no magic, no checksum, no identity.
    let agfl = if sb.is_v5() {
        read_header(
            device,
            sb,
            ag,
            start + 3 * sector,
            "AGFL",
            XFS_AGFL_MAGIC,
            agfl::V5_HEADER_LEN,
            Some(Identity {
                crc: agfl::offsets::CRC,
                uuid: agfl::offsets::UUID,
                seqno: agfl::offsets::SEQNO,
            }),
            |buf| agfl_slots(buf, agfl::V5_HEADER_LEN),
            |_, _, _| Vec::new(),
        )
    } else {
        read_header(
            device,
            sb,
            ag,
            start + 3 * sector,
            "AGFL",
            0,
            0,
            None,
            |buf| agfl_slots(buf, 0),
            |_, _, _| Vec::new(),
        )
    };
    AgHeaders { ag, agf, agi, agfl }
}

#[allow(clippy::too_many_arguments)]
fn read_header<T>(
    device: &dyn BlockRead,
    sb: &Superblock,
    ag: u32,
    offset: u64,
    name: &str,
    magic: u32,
    size: usize,
    identity: Option<Identity>,
    decode: impl Fn(&[u8]) -> T,
    ranges: impl Fn(&T, &Superblock, u32) -> Vec<String>,
) -> Header<T> {
    let sectsize = usize::from(sb.sectsize);
    let location = Extent::new(offset, sectsize as u64);
    let structure = format!("{name} {ag}");
    let mut header = Header {
        location,
        integrity: Integrity::Unchecked,
        value: None,
        damage: Vec::new(),
    };
    let finding =
        |kind, detail: String| finding(kind, Severity::Error, location, structure.clone(), detail);
    let mut buf = vec![0u8; sectsize.max(size)];
    if let Err(e) = device.read_at(offset, &mut buf[..sectsize]) {
        header
            .damage
            .push(finding(DamageKind::Unreadable, e.to_string()));
        return header;
    }
    let Some(identity) = identity else {
        header.value = Some(decode(&buf));
        return header;
    };
    let found = be32(&buf, 0);
    if found != magic {
        header.damage.push(finding(
            DamageKind::Signature,
            format!("magic {found:#010x}, expected {magic:#010x}"),
        ));
        return header;
    }
    if sb.is_v5() {
        let stored = le32(&buf, identity.crc);
        let computed = crc32c_with_zeroed_crc(&buf[..sectsize], identity.crc);
        header.integrity = if stored == computed {
            Integrity::Verified
        } else {
            header.damage.push(finding(
                DamageKind::Checksum,
                format!("stored CRC {stored:#010x}, computed {computed:#010x}"),
            ));
            Integrity::Failed
        };
        if uuid_at(&buf, identity.uuid) != sb.meta_uuid {
            header.damage.push(finding(
                DamageKind::Mismatch,
                "the UUID is not this volume's".to_string(),
            ));
        }
        let seqno = be32(&buf, identity.seqno);
        if seqno != ag {
            header.damage.push(finding(
                DamageKind::Mismatch,
                format!("it says it belongs to group {seqno}"),
            ));
        }
    }
    let value = decode(&buf);
    for detail in ranges(&value, sb, ag) {
        header.damage.push(finding(DamageKind::Malformed, detail));
    }
    header.value = Some(value);
    header
}

/// The range checks [`Agf::parse`] makes, as findings.
fn agf_ranges(agf: &Agf, sb: &Superblock, ag: u32) -> Vec<String> {
    let mut out = Vec::new();
    let expected = expected_length(sb, ag);
    if agf.length != expected {
        out.push(format!(
            "length {} blocks; the geometry gives this group {expected}",
            agf.length
        ));
    }
    if agf.freeblks > agf.length {
        out.push(format!(
            "{} free blocks in a group of {}",
            agf.freeblks, agf.length
        ));
    }
    if agf.longest > agf.length {
        out.push(format!(
            "longest free extent {} in a group of {}",
            agf.longest, agf.length
        ));
    }
    out
}

/// The range checks [`Agi::parse`] makes, as findings.
fn agi_ranges(agi: &Agi, sb: &Superblock, ag: u32) -> Vec<String> {
    let mut out = Vec::new();
    let expected = expected_length(sb, ag);
    if agi.length != expected {
        out.push(format!(
            "length {} blocks; the geometry gives this group {expected}",
            agi.length
        ));
    }
    if agi.freecount > agi.count {
        out.push(format!(
            "{} free inodes of {} allocated",
            agi.freecount, agi.count
        ));
    }
    if agi.level == 0 {
        out.push("the inode B+tree has level 0".to_string());
    }
    out
}

/// The length in blocks the geometry gives group `ag`: `agblocks`, but for
/// the last group, which holds what is left.
fn expected_length(sb: &Superblock, ag: u32) -> u32 {
    let start = u64::from(ag) * u64::from(sb.agblocks);
    let left = sb.dblocks.saturating_sub(start);
    u32::try_from(left.min(u64::from(sb.agblocks))).unwrap_or(sb.agblocks)
}

fn agfl_slots(buf: &[u8], header: usize) -> Vec<u32> {
    buf[header..]
        .chunks_exact(agfl::ENTRY_LEN)
        .map(|slot| u32::from_be_bytes([slot[0], slot[1], slot[2], slot[3]]))
        .collect()
}

/// What the log says about how the volume was left, as findings.
///
/// A log that needs replay is a notice, not damage: a mount replays it in
/// memory. Nothing here replays it, and nothing writes.
pub fn log_health(device: &dyn BlockRead, sb: &Superblock) -> Vec<Damage> {
    let location = Extent::new(
        if sb.has_internal_log() {
            sb.fsblock_offset(sb.logstart)
        } else {
            0
        },
        u64::from(sb.logblocks) * u64::from(sb.blocksize),
    );
    match log::inspect(device, sb) {
        Ok(LogState::Empty | LogState::CleanlyUnmounted) => Vec::new(),
        Ok(LogState::NeedsReplay) => vec![finding(
            DamageKind::Recorded,
            Severity::Notice,
            location,
            "log",
            "the volume was not unmounted cleanly: the log holds changes to replay",
        )],
        Err(e) => vec![finding(
            DamageKind::Unreadable,
            Severity::Warning,
            location,
            "log",
            e.to_string(),
        )],
    }
}

/// Every finding about a volume's headers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeDamage {
    pub superblocks: Superblocks,
    pub groups: Vec<AgHeaders>,
    /// The log's state, when there was a geometry to find it with.
    pub log: Vec<Damage>,
}

impl VolumeDamage {
    /// Every finding, superblocks first, then each group, then the log.
    pub fn all(&self) -> impl Iterator<Item = &Damage> {
        self.superblocks
            .all_damage()
            .chain(self.groups.iter().flat_map(AgHeaders::all_damage))
            .chain(self.log.iter())
    }
}

/// The superblocks, every group's headers and the log, checked.
pub fn damage(device: &dyn BlockRead) -> VolumeDamage {
    let superblocks = superblocks(device);
    let (groups, log) = match &superblocks.geometry {
        Some(sb) => (
            (0..group_count(sb))
                .map(|ag| ag_headers(device, sb, ag))
                .collect(),
            log_health(device, sb),
        ),
        None => (Vec::new(), Vec::new()),
    };
    VolumeDamage {
        superblocks,
        groups,
        log,
    }
}
