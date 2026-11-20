//! Inodes found in raw bytes, and the storage each one claims.

use super::evidence::{Damage, DamageKind, Extent, Integrity, Severity};
use crate::bmbt;
use crate::endian::{be16, be64, le32, uuid_at};
use crate::extent::{Extent as Record, EXTENT_RECORD_SIZE};
use crate::inode::{offsets, Format, Inode, XFS_DINODE_MAGIC};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};
use fs_core::BlockRead;

/// Bytes an inode scan reads at a time.
const SCAN_CHUNK: u64 = 1 << 20;

/// Whether an inode record says it holds a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum InodeState {
    /// The mode is set: the inode holds, or last held, a file the volume
    /// still counts.
    Live,
    /// The mode is zero: a free slot, or a deleted file's inode. XFS frees
    /// a file's blocks before it frees the inode, so a deleted file's
    /// record no longer maps its data.
    Free,
}

/// An inode record found in raw bytes. Evidence, not a trusted file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InodeCandidate {
    /// The inode number the record's position gives it. A v3 record also
    /// stores its own number; a disagreement is in `damage`.
    pub ino: u64,
    /// The record's bytes on the source.
    pub location: Extent,
    pub integrity: Integrity,
    pub state: InodeState,
    /// The core, decoded past a failed checksum or identity. `None` when a
    /// fork format is undefined, so that the fields' places are unknown.
    pub inode: Option<Inode>,
    pub damage: Vec<Damage>,
}

/// The outcome of an inode scan whose candidates went to a callback.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InodeScan {
    /// Candidates passed on.
    pub candidates: u64,
    /// Ranges inside the scan that could not be read. Nothing in them was
    /// decoded.
    pub unreadable: Vec<Extent>,
    /// Set when the callback stopped the scan: the first byte not scanned.
    pub stopped_at: Option<u64>,
}

/// The inode number of the record at byte `offset`, under the geometry
/// `sb`, or `None` when no inode can start there.
pub fn inode_number_at(sb: &Superblock, offset: u64) -> Option<u64> {
    let isize = u64::from(sb.inodesize);
    let block = u64::from(sb.blocksize);
    let group = u64::from(sb.agblocks) * block;
    if isize == 0 || group == 0 || !offset.is_multiple_of(isize) {
        return None;
    }
    let ag = u32::try_from(offset / group).ok()?;
    if ag >= sb.agcount {
        return None;
    }
    let within = offset % group;
    let agbno = within / block;
    let slot = (within % block) / isize;
    let agino = u32::try_from((agbno << sb.inopblog) | slot).ok()?;
    Some(sb.join_ino(ag, agino))
}

/// The byte offset of inode `ino` under the geometry `sb`.
pub fn inode_location(sb: &Superblock, ino: u64) -> Extent {
    let (ag, agbno, slot) = sb.split_ino(ino);
    let offset = sb
        .ag_offset(ag)
        .saturating_add(u64::from(agbno) * u64::from(sb.blocksize))
        .saturating_add(u64::from(slot) * u64::from(sb.inodesize));
    Extent::new(offset, u64::from(sb.inodesize))
}

/// Read inode `ino` where the geometry puts it, as a salvage candidate.
///
/// `None` when the record could not be read or holds no inode magic; the
/// caller asked for a number, and there is no inode there.
pub fn read_inode(device: &dyn BlockRead, sb: &Superblock, ino: u64) -> Option<InodeCandidate> {
    let location = inode_location(sb, ino);
    let mut buf = vec![0u8; location.length as usize];
    device.read_at(location.offset, &mut buf).ok()?;
    looks_like_inode(sb, &buf).then(|| candidate(sb, ino, location, &buf))
}

/// Whether `record` starts the way an inode on this volume does: the
/// magic, and a version the volume's inodes have.
fn looks_like_inode(sb: &Superblock, record: &[u8]) -> bool {
    record.len() > offsets::VERSION
        && be16(record, offsets::MAGIC) == XFS_DINODE_MAGIC
        && if sb.is_v5() {
            record[offsets::VERSION] == 3
        } else {
            matches!(record[offsets::VERSION], 1 | 2)
        }
}

/// Check and decode the record `buf`, which sits at `location` and should
/// be inode `ino`.
fn candidate(sb: &Superblock, ino: u64, location: Extent, buf: &[u8]) -> InodeCandidate {
    let structure = format!("inode {ino}");
    let mut damage = Vec::new();
    let mut finding = |kind, detail: String| {
        damage.push(Damage::new(
            kind,
            Severity::Error,
            location,
            structure.clone(),
            detail,
        ));
    };
    let isize = usize::from(sb.inodesize);
    let integrity = if sb.is_v5() {
        let stored = le32(buf, offsets::CRC);
        let computed = crc32c_with_zeroed_crc(&buf[..isize], offsets::CRC);
        let stated = be64(buf, offsets::INO);
        if stated != ino {
            finding(
                DamageKind::Mismatch,
                format!("the record says it is inode {stated}"),
            );
        }
        if uuid_at(buf, offsets::UUID) != sb.meta_uuid {
            finding(
                DamageKind::Mismatch,
                "the record's UUID is not this volume's".to_string(),
            );
        }
        if stored == computed {
            Integrity::Verified
        } else {
            finding(
                DamageKind::Checksum,
                format!("stored CRC {stored:#010x}, computed {computed:#010x}"),
            );
            Integrity::Failed
        }
    } else {
        Integrity::Unchecked
    };
    let inode = match Inode::decode_unverified(buf, sb, ino) {
        Ok(inode) => {
            if let Err(e) = inode.validate(sb) {
                finding(DamageKind::Malformed, e.to_string());
            }
            Some(inode)
        }
        Err(e) => {
            finding(DamageKind::Malformed, e.to_string());
            None
        }
    };
    let state = if be16(buf, offsets::MODE) == 0 {
        InodeState::Free
    } else {
        InodeState::Live
    };
    InodeCandidate {
        ino,
        location,
        integrity,
        state,
        inode,
        damage,
    }
}

/// Scan `range` for inode records, at every multiple of the inode size,
/// passing each one to `found` until it returns `false`.
///
/// Live, free and damaged records are all passed on. A record is
/// recognised by its magic and a version this volume's inodes have, so on
/// a v4 volume, which has no checksum, file data that happens to start
/// that way is a candidate too: its integrity says `Unchecked`.
pub fn find_inodes(
    device: &dyn BlockRead,
    sb: &Superblock,
    range: Extent,
    found: &mut dyn FnMut(InodeCandidate) -> bool,
) -> InodeScan {
    let isize = u64::from(sb.inodesize);
    let mut report = InodeScan::default();
    let Some(end) = range.end() else {
        return report;
    };
    let mut at = range.offset.next_multiple_of(isize);
    let chunk_len = SCAN_CHUNK - SCAN_CHUNK % isize;
    let mut chunk = vec![0u8; chunk_len as usize];
    while at.saturating_add(isize) <= end {
        let length = chunk_len.min((end - at) / isize * isize);
        let buf = &mut chunk[..length as usize];
        let whole = device.read_at(at, buf).is_ok();
        for slot in (0..length).step_by(isize as usize) {
            let offset = at + slot;
            let mut one;
            let record: &[u8] = if whole {
                &buf[slot as usize..(slot + isize) as usize]
            } else {
                one = vec![0u8; isize as usize];
                if device.read_at(offset, &mut one).is_err() {
                    push_merged(&mut report.unreadable, Extent::new(offset, isize));
                    continue;
                }
                &one
            };
            if !looks_like_inode(sb, record) {
                continue;
            }
            let Some(ino) = inode_number_at(sb, offset) else {
                continue;
            };
            report.candidates += 1;
            if !found(candidate(sb, ino, Extent::new(offset, isize), record)) {
                report.stopped_at = Some(offset + isize);
                return report;
            }
        }
        at += length;
    }
    report
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

/// One piece of a file's data on the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileExtent {
    /// Where in the file the piece starts, in bytes.
    pub file_offset: u64,
    /// The bytes on the source.
    pub location: Extent,
    /// Allocated but never written: the file reads zeros here, whatever
    /// the source holds.
    pub unwritten: bool,
}

/// Why an extent walk ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkEnd {
    /// Every part of the map was read and taken at its word. A sparse
    /// file's holes are not a break.
    Complete,
    /// Part of the map could not be used: a block of its tree failed a
    /// check, or a record did not decode.
    Broken,
    /// A record named blocks outside the volume.
    OutOfRange,
    /// The map's tree led back to a block already visited.
    Cycle,
    /// A block of the map could not be read.
    Unreadable(Extent),
}

/// The storage an inode claims, as far as its map could be followed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExtentMap {
    /// The data's pieces in file order, trimmed to the file's size.
    pub extents: Vec<FileExtent>,
    /// For data kept inside the inode (a short symbolic link or
    /// directory), the bytes on the source that hold it.
    pub inline: Option<Extent>,
    /// The size the inode records.
    pub claimed: u64,
    /// The blocks of the map's own B+tree that were read and used.
    pub map_blocks: Vec<Extent>,
    pub end: WalkEnd,
    pub damage: Vec<Damage>,
}

impl ExtentMap {
    /// Bytes the walk mapped onto the source.
    pub fn mapped(&self) -> u64 {
        self.extents.iter().map(|e| e.location.length).sum::<u64>()
            + self.inline.map_or(0, |e| e.length)
    }
}

/// Follow the data fork of `candidate`: the data inside the inode, its
/// extent list, or its block-map B+tree, keeping every extent reached.
///
/// The record is read again from the source, so the candidate can come
/// from anywhere: a scan, [`read_inode`], or a list kept from earlier.
pub fn walk_extents(
    device: &dyn BlockRead,
    sb: &Superblock,
    candidate: &InodeCandidate,
) -> ExtentMap {
    let structure = format!("inode {}", candidate.ino);
    let mut map = ExtentMap {
        extents: Vec::new(),
        inline: None,
        claimed: 0,
        map_blocks: Vec::new(),
        end: WalkEnd::Complete,
        damage: Vec::new(),
    };
    let Some(inode) = &candidate.inode else {
        map.end = WalkEnd::Broken;
        return map;
    };
    map.claimed = inode.size;
    let isize = usize::from(sb.inodesize);
    let mut buf = vec![0u8; isize];
    if let Err(e) = device.read_at(candidate.location.offset, &mut buf) {
        map.end = WalkEnd::Unreadable(candidate.location);
        map.damage.push(Damage::new(
            DamageKind::Unreadable,
            Severity::Error,
            candidate.location,
            structure,
            e.to_string(),
        ));
        return map;
    }
    let (start, fork_end) = inode.data_fork_range(isize);
    let fork = &buf[start.min(fork_end)..fork_end];
    if inode.is_realtime() && matches!(inode.format, Format::Extents | Format::Btree) {
        map.end = WalkEnd::Broken;
        map.damage.push(Damage::new(
            DamageKind::Missing,
            Severity::Warning,
            candidate.location,
            structure,
            "the data is on the realtime device, which is not this source",
        ));
        return map;
    }
    let records = match inode.format {
        Format::Local => {
            let length = inode.size.min(fork.len() as u64);
            if inode.size > fork.len() as u64 {
                map.end = WalkEnd::Broken;
                map.damage.push(Damage::new(
                    DamageKind::Malformed,
                    Severity::Error,
                    candidate.location,
                    structure,
                    format!(
                        "{} bytes of inline data in a fork of {}",
                        inode.size,
                        fork.len()
                    ),
                ));
            }
            map.inline = Some(Extent::new(
                candidate.location.offset + start as u64,
                length,
            ));
            return map;
        }
        Format::Extents => extent_list(fork, inode.nextents, &structure, candidate, &mut map),
        Format::Btree => {
            let walk = bmbt::walk_salvage(fork, sb, candidate.ino, |fsblock| {
                if !sb.extent_in_bounds(fsblock, 1) {
                    return None;
                }
                let mut block = vec![0u8; sb.blocksize as usize];
                device
                    .read_at(sb.fsblock_offset(fsblock), &mut block)
                    .ok()
                    .map(|()| block)
            });
            for (fsblock, kind, detail) in walk.problems {
                let location = match fsblock {
                    Some(b) => Extent::new(sb.fsblock_offset(b), u64::from(sb.blocksize)),
                    None => candidate.location,
                };
                let kind = if kind == DamageKind::Unreadable
                    && fsblock.is_some_and(|b| !sb.extent_in_bounds(b, 1))
                {
                    map.end = worse(map.end, WalkEnd::OutOfRange);
                    DamageKind::Malformed
                } else {
                    // A block that fails only its checksum is still
                    // decoded and its records used: that is damage to
                    // report, not a break in the map.
                    match kind {
                        DamageKind::Unreadable => {
                            map.end = worse(map.end, WalkEnd::Unreadable(location));
                        }
                        DamageKind::Checksum => {}
                        _ => map.end = worse(map.end, WalkEnd::Broken),
                    }
                    kind
                };
                map.damage.push(Damage::new(
                    kind,
                    Severity::Error,
                    location,
                    format!("{structure} block map"),
                    detail,
                ));
            }
            map.map_blocks = walk
                .blocks
                .iter()
                .map(|&b| Extent::new(sb.fsblock_offset(b), u64::from(sb.blocksize)))
                .collect();
            if walk.cycle {
                map.end = worse(map.end, WalkEnd::Cycle);
            }
            walk.extents
        }
        // A device inode holds a device number; the other formats hold no
        // file data.
        _ => Vec::new(),
    };
    place(sb, records, inode.size, &structure, candidate, &mut map);
    map
}

/// The records of an in-inode extent list, each decoded on its own.
fn extent_list(
    fork: &[u8],
    nextents: u64,
    structure: &str,
    candidate: &InodeCandidate,
    map: &mut ExtentMap,
) -> Vec<Record> {
    let room = (fork.len() / EXTENT_RECORD_SIZE) as u64;
    let count = if nextents > room {
        map.end = worse(map.end, WalkEnd::Broken);
        map.damage.push(Damage::new(
            DamageKind::Malformed,
            Severity::Error,
            candidate.location,
            structure,
            format!("{nextents} extents in a fork with room for {room}"),
        ));
        room
    } else {
        nextents
    };
    let mut out = Vec::new();
    for i in 0..count as usize {
        match Record::parse(&fork[i * EXTENT_RECORD_SIZE..]) {
            Ok(record) => out.push(record),
            Err(e) => {
                map.end = worse(map.end, WalkEnd::Broken);
                map.damage.push(Damage::new(
                    DamageKind::Malformed,
                    Severity::Error,
                    candidate.location,
                    structure,
                    format!("extent {i}: {e}"),
                ));
            }
        }
    }
    out
}

/// Put the records on the source in file order, trimmed to `size`,
/// keeping only those inside the volume.
fn place(
    sb: &Superblock,
    mut records: Vec<Record>,
    size: u64,
    structure: &str,
    candidate: &InodeCandidate,
    map: &mut ExtentMap,
) {
    let block = u64::from(sb.blocksize);
    records.sort_by_key(|r| r.startoff);
    let mut covered = 0u64;
    for record in records {
        if !sb.extent_in_bounds(record.startblock, record.blockcount) {
            map.end = worse(map.end, WalkEnd::OutOfRange);
            map.damage.push(Damage::new(
                DamageKind::Malformed,
                Severity::Error,
                candidate.location,
                structure,
                format!(
                    "extent at file block {} names {} blocks from {}, outside the volume",
                    record.startoff, record.blockcount, record.startblock
                ),
            ));
            continue;
        }
        let file_offset = record.startoff.saturating_mul(block);
        if file_offset < covered {
            map.end = worse(map.end, WalkEnd::Broken);
            map.damage.push(Damage::new(
                DamageKind::Malformed,
                Severity::Error,
                candidate.location,
                structure,
                format!(
                    "extent at file block {} overlaps the one before it",
                    record.startoff
                ),
            ));
            continue;
        }
        if file_offset >= size {
            // Blocks allocated past the end of the file: not its data.
            continue;
        }
        let length = record
            .blockcount
            .saturating_mul(block)
            .min(size - file_offset);
        covered = file_offset + length;
        map.extents.push(FileExtent {
            file_offset,
            location: Extent::new(sb.fsblock_offset(record.startblock), length),
            unwritten: record.unwritten,
        });
    }
}

/// The more telling of two reasons a walk ended.
fn worse(current: WalkEnd, next: WalkEnd) -> WalkEnd {
    let rank = |end: &WalkEnd| match end {
        WalkEnd::Complete => 0,
        WalkEnd::Broken => 1,
        WalkEnd::OutOfRange => 2,
        WalkEnd::Cycle => 3,
        WalkEnd::Unreadable(_) => 4,
    };
    if rank(&next) > rank(&current) {
        next
    } else {
        current
    }
}

/// Read a file's bytes through its map, starting `offset` bytes into the
/// file, up to the end of `buf` or of the file. Returns how many bytes were
/// placed and the file ranges among them that could not be read.
///
/// Holes and unwritten pieces read as zeros. So do bytes that could not be
/// read: the returned ranges, not the zeros, are the evidence.
pub fn read_mapped(
    device: &dyn BlockRead,
    map: &ExtentMap,
    offset: u64,
    buf: &mut [u8],
) -> (usize, Vec<Extent>) {
    let mut unreadable = Vec::new();
    let size = match map.inline {
        Some(inline) => inline.length,
        None => map.claimed,
    };
    if offset >= size {
        return (0, unreadable);
    }
    let wanted = (buf.len() as u64).min(size - offset) as usize;
    let buf = &mut buf[..wanted];
    buf.fill(0);
    let pieces: Vec<FileExtent> = match map.inline {
        Some(inline) => vec![FileExtent {
            file_offset: 0,
            location: inline,
            unwritten: false,
        }],
        None => map.extents.clone(),
    };
    let end = offset + wanted as u64;
    for piece in pieces.iter().filter(|p| !p.unwritten) {
        let piece_end = piece.file_offset + piece.location.length;
        let from = piece.file_offset.max(offset);
        let to = piece_end.min(end);
        if from >= to {
            continue;
        }
        let slice = &mut buf[(from - offset) as usize..(to - offset) as usize];
        let source = piece.location.offset + (from - piece.file_offset);
        if device.read_at(source, slice).is_err() {
            read_by_sector(device, source, from, slice, &mut unreadable);
        }
    }
    (wanted, unreadable)
}

/// The smallest unit a device fails a read in.
const SECTOR: u64 = 512;

/// Read `slice` from `source` a sector at a time, after a whole read of it
/// failed, so one bad sector or the device's end loses only what it must.
/// Sectors that do not read are zeros, and their file ranges, starting at
/// file offset `from`, are added to `unreadable`, merged where adjacent.
fn read_by_sector(
    device: &dyn BlockRead,
    source: u64,
    from: u64,
    slice: &mut [u8],
    unreadable: &mut Vec<Extent>,
) {
    let mut done = 0usize;
    while done < slice.len() {
        let at = source + done as u64;
        // Up to the next sector boundary of the source.
        let step = (SECTOR - at % SECTOR).min((slice.len() - done) as u64) as usize;
        let part = &mut slice[done..done + step];
        if device.read_at(at, part).is_err() {
            part.fill(0);
            let range = Extent::new(from + done as u64, step as u64);
            match unreadable.last_mut() {
                Some(last) if last.end() == Some(range.offset) => last.length += range.length,
                _ => unreadable.push(range),
            }
        }
        done += step;
    }
}
