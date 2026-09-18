//! Applying what the log holds, into memory (#90).
//!
//! A volume that was not unmounted cleanly is holding metadata in its
//! log that the filesystem itself has never been given. Every structure
//! on disk may be a version the log was about to replace, which is why
//! reading one without replaying first is the worst thing a read-only
//! driver can do: directories parse, checksums verify, and the answer is
//! simply old.
//!
//! So this replays. The log's records are read from the tail the newest
//! record names up to that record itself, their items are applied in the
//! order they were written, and the result goes into an
//! [`Overlay`](crate::overlay::Overlay) — **in memory**. The device is
//! not written to. A read-only mount stays read-only, which is what
//! recovering from a disk that must not be touched needs, and it is the
//! same layer the write path keeps its own unreplayed records in.
//!
//! # What recovery actually consists of
//!
//! Two passes over the same range, as the kernel does:
//!
//! 1. **Cancellations.** A block that was freed and reused carries a
//!    `BLF_CANCEL` item, and every earlier item for that block must not
//!    be applied — the block is not what it was. Knowing which blocks
//!    those are means reading the whole range first.
//! 2. **Application.** Buffer items write their logged chunks, inode
//!    items write their core and forks, and an icreate item initialises
//!    a chunk of inodes the record did not carry.
//!
//! # What is not applied, and why that is honest
//!
//! Intent items — `XFS_LI_EFI` and its relatives — record an operation
//! the kernel had begun and would finish after recovery: freeing an
//! extent, updating a reverse map. Finishing one means allocator work
//! against structures this replay has just rebuilt, and a read-only
//! mount has nowhere to put the result. They are counted rather than
//! applied, and what they leave unsettled is space accounting: blocks
//! that are free but still counted, a reverse map with a record too
//! many. Nothing a directory walk or a file read can see, and
//! [`Recovered::deferred`] says how many there were.

use crate::error::{Error, Result};
use crate::format::log_items::{
    buf_log_format, inode_log_format, item_types, log_dinode, op_header, rec_header, trans_header,
    BBSIZE,
};
use crate::overlay::Overlay;
use crate::superblock::Superblock;
use fs_core::BlockRead;
use std::collections::{BTreeMap, HashMap};

/// What a replay found and did.
#[derive(Debug, Default, Clone)]
pub(crate) struct Recovered {
    /// Records read between the tail and the head.
    pub records: usize,
    /// Transactions that reached their commit operation.
    pub transactions: usize,
    /// Buffer items applied.
    pub buffers: usize,
    /// Inode items applied.
    pub inodes: usize,
    /// Inode chunks initialised from an icreate item.
    pub chunks: usize,
    /// Item types this replay does not apply, and how many of each. See
    /// the module documentation for what they leave unsettled.
    pub deferred: BTreeMap<u16, usize>,
}

/// The log as a ring of basic blocks, read wherever the read lands.
struct Ring<'a> {
    device: &'a dyn BlockRead,
    /// Byte offset of the log's first block on the device.
    start: u64,
    /// Basic blocks in the ring.
    blocks: u64,
}

impl Ring<'_> {
    /// `count` basic blocks from `at`, wrapping at the end of the ring.
    ///
    /// A record may straddle the wrap — this driver's own writer pads
    /// rather than straddle, but the kernel's does not — so a read that
    /// stopped at the end of the ring would return the blocks a record
    /// was written over.
    fn read(&self, at: u64, count: u64) -> Result<Vec<u8>> {
        if count > self.blocks {
            return Err(Error::CorruptLog(format!(
                "a log record claims {count} basic blocks, more than the {} in the whole ring",
                self.blocks
            )));
        }
        let mut out = vec![0u8; (count * BBSIZE as u64) as usize];
        let mut block = at % self.blocks;
        let mut done = 0usize;
        while done < out.len() {
            let here = (((self.blocks - block) * BBSIZE as u64) as usize).min(out.len() - done);
            self.device.read_at(
                self.start + block * BBSIZE as u64,
                &mut out[done..done + here],
            )?;
            done += here;
            block = 0;
        }
        Ok(out)
    }
}

/// One item as the record holds it: its format operation, then the
/// regions it said would follow.
struct Item {
    regions: Vec<Vec<u8>>,
    /// Regions this item occupies in total, from the `size` field every
    /// item format carries at offset 2 — the format operation included.
    total: usize,
}

impl Item {
    fn kind(&self) -> u16 {
        let head = &self.regions[0];
        if head.len() < 2 {
            return 0;
        }
        u16::from_ne_bytes(head[0..2].try_into().expect("2 bytes"))
    }
}

/// Read the log from the tail to the head, handing each committed
/// transaction's items to `visit` in the order they were written.
fn walk<F>(
    device: &dyn BlockRead,
    sb: &Superblock,
    newest: &crate::log::Record,
    mut visit: F,
) -> Result<usize>
where
    F: FnMut(&[Item]) -> Result<()>,
{
    use rec_header::offsets as h;

    let (start, log_bytes) = crate::log::extent(sb)?;
    let ring = Ring {
        device,
        start,
        blocks: log_bytes / BBSIZE as u64,
    };

    // WHERE TO BEGIN IS THE NEWEST RECORD'S BUSINESS. `h_tail_lsn` is
    // the sequence number of the oldest record whose items the kernel
    // had not yet written to their own homes; everything before it is
    // already on disk, and everything from it onwards may not be.
    let tail_lsn = crate::endian::be64(&newest.header, h::TAIL_LSN);
    if tail_lsn == 0 {
        return Err(Error::CorruptLog(
            "the newest log record names no tail, so there is no telling which records \
             still have to be applied"
                .into(),
        ));
    }
    let mut at = tail_lsn & 0xffff_ffff;
    if at >= ring.blocks {
        return Err(Error::CorruptLog(format!(
            "the log's tail is at block {at}, past the {} blocks of the ring",
            ring.blocks
        )));
    }

    let mut in_flight: HashMap<u32, Vec<Item>> = HashMap::new();
    let mut records = 0usize;
    loop {
        let header = ring.read(at, 1)?;
        if crate::endian::be32(&header, h::MAGICNO) != crate::log::XLOG_HEADER_MAGIC
            || crate::endian::uuid_at(&header, h::FS_UUID) != sb.uuid
        {
            return Err(Error::CorruptLog(format!(
                "no log record begins at basic block {at}, where the record before it \
                 said the next one would"
            )));
        }
        let lsn = crate::endian::be64(&header, h::LSN);
        let header_blocks = crate::log::header_blocks(&header);
        let len = crate::endian::be32(&header, h::LEN) as usize;
        let data_blocks = (len as u64).div_ceil(BBSIZE as u64);

        // THE PAYLOAD AS IT WAS WRITTEN, for the checksum, and then as
        // it was meant: the writer replaced the first four bytes of
        // every basic block with the cycle number and kept the
        // displaced word in the header, so that a reader can tell a
        // freshly written block from the stale one beside it.
        let raw = ring.read(at + header_blocks, data_blocks)?;
        let headers = ring.read(at, header_blocks)?;
        check_record(&headers, header_blocks as usize, &raw[..len], at)?;
        let data = unpack(&headers, header_blocks as usize, raw, len);

        read_ops(
            &data,
            crate::endian::be32(&header, h::NUM_LOGOPS),
            &mut in_flight,
            &mut visit,
        )?;
        records += 1;

        if lsn == newest.lsn {
            break;
        }
        at = (at + header_blocks + data_blocks) % ring.blocks;
        if records as u64 > ring.blocks {
            return Err(Error::CorruptLog(
                "the log's records loop without ever reaching its newest".into(),
            ));
        }
    }
    Ok(records)
}

/// Refuse a record whose checksum does not match what it holds.
///
/// A record is only durable once its checksum verifies, which is what
/// lets a crash mid-write be recognised rather than replayed: the
/// kernel discards such a record and everything after it. Here it is a
/// refusal instead — this driver is reading a volume rather than taking
/// it over, and a record it cannot trust in the middle of the range is
/// not something to quietly skip past.
///
/// A zero checksum is not checked: a version-1 log does not carry one.
fn check_record(headers: &[u8], header_blocks: usize, data: &[u8], at: u64) -> Result<()> {
    let stored = u32::from_le_bytes(
        headers[rec_header::offsets::CRC..rec_header::offsets::CRC + 4]
            .try_into()
            .expect("4 bytes"),
    );
    if stored == 0 {
        return Ok(());
    }
    // The checksum covers the header struct — not the whole basic block
    // it sits in — then each further header block's struct, then the
    // payload as it was written.
    let mut buf = headers[..rec_header::XLOG_REC_HEADER_SIZE].to_vec();
    buf[rec_header::offsets::CRC..rec_header::offsets::CRC + 4].copy_from_slice(&[0; 4]);
    for block in 1..header_blocks {
        let from = block * BBSIZE;
        buf.extend_from_slice(&headers[from..from + rec_header::XLOG_REC_HEADER_SIZE]);
    }
    buf.extend_from_slice(data);
    let computed = crc32c::crc32c(&buf);
    if computed != stored {
        return Err(Error::CorruptLog(format!(
            "the log record at basic block {at} says its checksum is {stored:#010x} and \
             its contents give {computed:#010x}"
        )));
    }
    Ok(())
}

/// Put the displaced first word of each basic block back.
fn unpack(headers: &[u8], header_blocks: usize, mut data: Vec<u8>, len: usize) -> Vec<u8> {
    let per_header = rec_header::XLOG_CYCLE_DATA_ENTRIES;
    for i in 0..data.len() / BBSIZE {
        let word = if i < per_header {
            rec_header::offsets::CYCLE_DATA + 4 * i
        } else {
            // A header describing more than 32 KiB of log spills into
            // further blocks, each holding its own cycle-data array
            // four bytes in.
            let block = i / per_header;
            if block >= header_blocks {
                break;
            }
            block * BBSIZE + 4 + 4 * (i % per_header)
        };
        let at = i * BBSIZE;
        data[at..at + 4].copy_from_slice(&headers[word..word + 4]);
    }
    data.truncate(len);
    data
}

/// Split a record's payload into operations, and gather them into the
/// transactions they belong to.
fn read_ops<F>(
    data: &[u8],
    num_logops: u32,
    in_flight: &mut HashMap<u32, Vec<Item>>,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(&[Item]) -> Result<()>,
{
    use op_header::offsets as o;

    let mut at = 0usize;
    for _ in 0..num_logops {
        if at + op_header::OP_HEADER_SIZE > data.len() {
            return Err(Error::CorruptLog(
                "a log record holds fewer operations than its header counts".into(),
            ));
        }
        let tid = crate::endian::be32(data, at + o::TID);
        let len = crate::endian::be32(data, at + o::LEN) as usize;
        let flags = data[at + o::FLAGS];
        at += op_header::OP_HEADER_SIZE;
        if at + len > data.len() {
            return Err(Error::CorruptLog(format!(
                "a log operation claims {len} bytes, which runs past the record holding it"
            )));
        }
        let body = &data[at..at + len];
        at += len;

        if flags & op_header::XLOG_UNMOUNT_TRANS != 0 {
            continue;
        }
        if flags & op_header::XLOG_START_TRANS != 0 {
            in_flight.insert(tid, Vec::new());
            continue;
        }
        if flags & op_header::XLOG_COMMIT_TRANS != 0 {
            // A COMMIT FOR A TRANSACTION THAT STARTED BEFORE THE TAIL is
            // nothing to apply. The tail is where the oldest *unapplied*
            // item is; a transaction whose start lies before it was
            // written to its home long ago.
            if let Some(items) = in_flight.remove(&tid) {
                if !items.is_empty() {
                    visit(&items)?;
                }
            }
            continue;
        }
        let Some(items) = in_flight.get_mut(&tid) else {
            continue;
        };
        add_region(items, body, flags);
    }
    Ok(())
}

/// Add one operation's bytes to the transaction it belongs to.
fn add_region(items: &mut Vec<Item>, body: &[u8], flags: u8) {
    // A REGION TOO BIG FOR ONE RECORD IS SPLIT ACROSS TWO. The second
    // half arrives as operation 0 of the next record and belongs on the
    // end of the first, not in a region of its own — a directory block
    // logged across a record boundary would otherwise be read as two
    // items, and the second would be read as an item format.
    //
    // Both bits of `XLOG_OP_CONTINUATION` are tested together, as
    // `log_items` asks: neither was ever seen alone, so which carries
    // which meaning is not established.
    if flags & op_header::XLOG_OP_CONTINUATION != 0 {
        if let Some(region) = items.last_mut().and_then(|i| i.regions.last_mut()) {
            region.extend_from_slice(body);
        }
        return;
    }
    if body.is_empty() {
        return;
    }
    // The first operation of a transaction is its header, which names
    // the transaction rather than describing a change.
    if items.is_empty()
        && body.len() >= trans_header::TRANS_HEADER_SIZE
        && u32::from_ne_bytes(body[0..4].try_into().expect("4 bytes"))
            == trans_header::XFS_TRANS_HEADER_MAGIC
    {
        return;
    }
    match items.last_mut() {
        Some(item) if item.regions.len() < item.total => item.regions.push(body.to_vec()),
        _ => {
            // Every item format begins with its type and then the number
            // of operations it occupies, this one included.
            let total = if body.len() >= 4 {
                usize::from(u16::from_ne_bytes(body[2..4].try_into().expect("2 bytes")))
            } else {
                1
            };
            items.push(Item {
                regions: vec![body.to_vec()],
                total: total.max(1),
            });
        }
    }
}

/// Replay everything the log holds into `into`, and say what that was.
pub(crate) fn replay(device: &dyn BlockRead, sb: &Superblock, into: &Overlay) -> Result<Recovered> {
    let (start, log_bytes) = crate::log::extent(sb)?;
    let Some(newest) = crate::log::scan_for_newest_record(device, sb, start, log_bytes)? else {
        return Ok(Recovered::default());
    };

    // PASS ONE: which blocks were cancelled.
    //
    // A block that was freed and handed out again is logged with
    // `BLF_CANCEL` at the point it changed hands. Items for it earlier
    // in the same range describe what it used to be, and applying one
    // would write a directory block over an inode chunk. Knowing which
    // they are means reading the range through before applying any of
    // it.
    let mut cancelled: HashMap<(u64, u32), usize> = HashMap::new();
    walk(device, sb, &newest, |items| {
        for item in items {
            if item.kind() != item_types::XFS_LI_BUF {
                continue;
            }
            let f = &item.regions[0];
            if f.len() < buf_log_format::BLF_HEADER_SIZE {
                continue;
            }
            if native_u16(f, buf_log_format::offsets::FLAGS) & buf_log_format::flags::BLF_CANCEL
                == 0
            {
                continue;
            }
            *cancelled
                .entry((
                    native_u64(f, buf_log_format::offsets::BLKNO),
                    u32::from(native_u16(f, buf_log_format::offsets::LEN)),
                ))
                .or_default() += 1;
        }
        Ok(())
    })?;

    // PASS TWO: apply, in the order the records were written.
    let mut out = Recovered::default();
    out.records = walk(device, sb, &newest, |items| {
        out.transactions += 1;
        for item in items {
            match item.kind() {
                item_types::XFS_LI_BUF => {
                    if apply_buffer(sb, into, item, &mut cancelled)? {
                        out.buffers += 1;
                    }
                }
                item_types::XFS_LI_INODE => {
                    if apply_inode(sb, into, item, &cancelled)? {
                        out.inodes += 1;
                    }
                }
                item_types::XFS_LI_ICREATE => {
                    apply_icreate(sb, into, item)?;
                    out.chunks += 1;
                }
                other => *out.deferred.entry(other).or_default() += 1,
            }
        }
        Ok(())
    })?;
    Ok(out)
}

fn native_u16(buf: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes(buf[at..at + 2].try_into().expect("2 bytes"))
}

fn native_u32(buf: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(buf[at..at + 4].try_into().expect("4 bytes"))
}

fn native_u64(buf: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(buf[at..at + 8].try_into().expect("8 bytes"))
}

/// Whether a block is one a later item takes over, and consume one
/// cancellation if this item is the one doing the taking.
fn is_cancelled(cancelled: &HashMap<(u64, u32), usize>, blkno: u64, len: u32) -> bool {
    cancelled.contains_key(&(blkno, len))
}

/// Apply one buffer item, and say whether it was applied at all.
fn apply_buffer(
    sb: &Superblock,
    into: &Overlay,
    item: &Item,
    cancelled: &mut HashMap<(u64, u32), usize>,
) -> Result<bool> {
    use buf_log_format::offsets as b;

    let f = &item.regions[0];
    if f.len() < buf_log_format::BLF_HEADER_SIZE {
        return Err(Error::CorruptLog(
            "a buffer log item is shorter than its own header".into(),
        ));
    }
    let flags = native_u16(f, b::FLAGS);
    let blkno = native_u64(f, b::BLKNO);
    let len = u32::from(native_u16(f, b::LEN));
    let buf_type = flags >> buf_log_format::BLF_TYPE_SHIFT;

    // THE CANCEL ITSELF IS CONSUMED HERE. A block can be freed and
    // reused more than once in the same range, and the items between
    // two cancellations describe the block as it was in between — so a
    // cancellation suppresses replay only until the item that placed it
    // is reached.
    if flags & buf_log_format::flags::BLF_CANCEL != 0 {
        if let Some(count) = cancelled.get_mut(&(blkno, len)) {
            *count -= 1;
            if *count == 0 {
                cancelled.remove(&(blkno, len));
            }
        }
        return Ok(false);
    }
    if is_cancelled(cancelled, blkno, len) {
        return Ok(false);
    }
    if len == 0 {
        return Ok(false);
    }

    let map_size = native_u32(f, b::MAP_SIZE) as usize;
    let map_end = buf_log_format::offsets::DATA_MAP + 4 * map_size;
    if f.len() < map_end {
        return Err(Error::CorruptLog(format!(
            "a buffer log item says its bitmap is {map_size} words, which does not fit \
             the {} bytes of the item",
            f.len()
        )));
    }
    let map: Vec<u32> = (0..map_size)
        .map(|i| native_u32(f, buf_log_format::offsets::DATA_MAP + 4 * i))
        .collect();

    let at = blkno * BBSIZE as u64;
    let mut buffer = vec![0u8; (len as usize) * BBSIZE];
    into.read_at(at, &mut buffer)?;

    // The bitmap's runs and the item's regions correspond one to one:
    // each run of set bits is one operation's worth of bytes.
    let runs = runs_of(&map, buffer.len() / buf_log_format::BLF_CHUNK);
    if flags & buf_log_format::flags::BLF_INODE_BUF != 0 {
        apply_inode_buffer(sb, &mut buffer, item, &runs);
    } else {
        for (run, region) in runs.iter().zip(item.regions.iter().skip(1)) {
            let from = run.0 * buf_log_format::BLF_CHUNK;
            let take = (run.1 * buf_log_format::BLF_CHUNK)
                .min(region.len())
                .min(buffer.len().saturating_sub(from));
            buffer[from..from + take].copy_from_slice(&region[..take]);
        }
    }

    stamp(sb, buf_type, &mut buffer);
    into.wrote(at, &buffer);
    Ok(true)
}

/// The runs of set bits in a buffer item's bitmap, as (first chunk,
/// chunks).
fn runs_of(map: &[u32], chunks: usize) -> Vec<(usize, usize)> {
    let set = |bit: usize| -> bool {
        map.get(bit / 32)
            .is_some_and(|word| word & (1u32 << (bit % 32)) != 0)
    };
    let mut out = Vec::new();
    let mut bit = 0;
    let end = (map.len() * 32).min(chunks);
    while bit < end {
        if !set(bit) {
            bit += 1;
            continue;
        }
        let start = bit;
        while bit < end && set(bit) {
            bit += 1;
        }
        out.push((start, bit - start));
    }
    out
}

/// Apply an inode cluster's buffer item, which is a special case.
///
/// The chunks such an item logs hold a stale copy of the inodes around
/// the field that actually changed: the unlinked-list pointer, which the
/// kernel maintains through the buffer rather than through the inode
/// item beside it. Writing the chunks whole would put an old inode core
/// back over a new one — so only the four bytes of `di_next_unlinked`
/// are taken from each logged chunk, exactly as the kernel's
/// `xlog_recover_do_inode_buffer` does.
fn apply_inode_buffer(sb: &Superblock, buffer: &mut [u8], item: &Item, runs: &[(usize, usize)]) {
    const NEXT_UNLINKED: usize = crate::format::log_items::log_dinode::offsets::NEXT_UNLINKED;
    let inodesize = usize::from(sb.inodesize);
    if inodesize == 0 {
        return;
    }
    for slot in 0..buffer.len() / inodesize {
        let field = slot * inodesize + NEXT_UNLINKED;
        let bit = field / buf_log_format::BLF_CHUNK;
        let Some((index, run)) = runs
            .iter()
            .enumerate()
            .find(|(_, (start, count))| bit >= *start && bit < start + count)
        else {
            continue;
        };
        let Some(region) = item.regions.get(index + 1) else {
            continue;
        };
        let within = field - run.0 * buf_log_format::BLF_CHUNK;
        if within + 4 > region.len() || field + 4 > buffer.len() {
            continue;
        }
        buffer[field..field + 4].copy_from_slice(&region[within..within + 4]);
    }
}

/// Apply one inode item, and say whether it was applied.
fn apply_inode(
    sb: &Superblock,
    into: &Overlay,
    item: &Item,
    cancelled: &HashMap<(u64, u32), usize>,
) -> Result<bool> {
    use inode_log_format::offsets as i;

    let f = &item.regions[0];
    if f.len() < inode_log_format::INODE_LOG_FORMAT_SIZE {
        return Err(Error::CorruptLog(
            "an inode log item is shorter than its own format".into(),
        ));
    }
    let fields = native_u32(f, i::FIELDS);
    let blkno = native_u64(f, i::BLKNO);
    let len = native_u32(f, i::LEN);
    let boffset = native_u32(f, i::BOFFSET) as usize;
    let dsize = usize::from(native_u16(f, i::DSIZE));
    let asize = usize::from(native_u16(f, i::ASIZE));

    // An inode whose cluster was freed and reused is not an inode any
    // more, and its item describes what used to be there.
    if is_cancelled(cancelled, blkno, len) {
        return Ok(false);
    }

    let at = blkno * BBSIZE as u64 + boffset as u64;
    let mut inode = vec![0u8; usize::from(sb.inodesize)];
    into.read_at(at, &mut inode)?;

    let mut region = 1;
    if fields & inode_log_format::XFS_ILOG_CORE != 0 {
        let Some(core) = item.regions.get(region) else {
            return Err(Error::CorruptLog(
                "an inode item says it logs a core and does not carry one".into(),
            ));
        };
        region += 1;
        let disk = crate::log_write::log_dinode_to_disk(core)
            .map_err(|why| Error::CorruptLog(format!("a logged inode core: {why}")))?;
        if disk.len() > inode.len() {
            return Err(Error::CorruptLog(format!(
                "a logged inode core is {} bytes against an inode of {}",
                disk.len(),
                inode.len()
            )));
        }
        // THE UNLINKED POINTER IS NOT THE CORE'S TO GIVE. It is always
        // the sentinel in a logged core, and it is maintained through
        // the inode *buffer* item — so taking it from here would undo a
        // buffer item that ran earlier in the same record.
        let keep = inode
            [log_dinode::offsets::NEXT_UNLINKED..log_dinode::offsets::NEXT_UNLINKED + 4]
            .to_vec();
        inode[..disk.len()].copy_from_slice(&disk);
        if disk.len() >= log_dinode::offsets::NEXT_UNLINKED + 4 {
            inode[log_dinode::offsets::NEXT_UNLINKED..log_dinode::offsets::NEXT_UNLINKED + 4]
                .copy_from_slice(&keep);
        }
    }

    // A FORK STAYS BIG-ENDIAN INSIDE A NATIVE-ENDIAN RECORD, so it is
    // copied rather than converted. Its home is the literal area after
    // the core, and the attribute fork's is `di_forkoff` eight-byte
    // words further on.
    let fork_start = if inode[crate::inode::offsets::VERSION] >= 3 {
        crate::inode::XFS_DINODE_V3_SIZE
    } else {
        crate::inode::XFS_DINODE_V2_SIZE
    };
    if fields & FORK_FIELDS != 0 && dsize > 0 {
        let Some(fork) = item.regions.get(region) else {
            return Err(Error::CorruptLog(
                "an inode item says it logs a data fork and does not carry one".into(),
            ));
        };
        region += 1;
        let take = dsize.min(fork.len()).min(inode.len() - fork_start);
        inode[fork_start..fork_start + take].copy_from_slice(&fork[..take]);
    }
    if fields & ATTR_FORK_FIELDS != 0 && asize > 0 {
        if let Some(fork) = item.regions.get(region) {
            let forkoff = usize::from(inode[crate::inode::offsets::FORKOFF]) * 8;
            let start = fork_start + forkoff;
            if start < inode.len() {
                let take = asize.min(fork.len()).min(inode.len() - start);
                inode[start..start + take].copy_from_slice(&fork[..take]);
            }
        }
    }

    stamp_inode(sb, &mut inode);
    into.wrote(at, &inode);
    Ok(true)
}

/// `XFS_ILOG_DDATA | XFS_ILOG_DEXT | XFS_ILOG_DBROOT` — the three ways a
/// data fork is logged. Only one is ever set at a time.
const FORK_FIELDS: u32 = 0x02 | 0x04 | 0x08;

/// `XFS_ILOG_ADATA | XFS_ILOG_AEXT | XFS_ILOG_ABROOT`, the same for the
/// attribute fork.
const ATTR_FORK_FIELDS: u32 = 0x10 | 0x20 | 0x40;

/// Initialise a chunk of inodes an icreate item names.
///
/// The item carries no inodes: 64 of them is 32 KiB, which would dwarf
/// the transaction that allocated them, so the record says what to make
/// and recovery makes it. Every inode comes out identical apart from its
/// own number.
fn apply_icreate(sb: &Superblock, into: &Overlay, item: &Item) -> Result<()> {
    use crate::format::log_items::icreate_log_format::SIZE;

    let f = &item.regions[0];
    if f.len() < SIZE {
        return Err(Error::CorruptLog(
            "an icreate item is shorter than its own format".into(),
        ));
    }
    // Every field past the two `u16`s is big-endian, unlike the buffer
    // and inode formats beside it.
    let be = |at: usize| crate::endian::be32(f, at);
    let (agno, agbno, count, isize, length, gen) = (be(4), be(8), be(12), be(16), be(20), be(24));

    if agno >= sb.agcount {
        return Err(Error::CorruptLog(format!(
            "an icreate item names allocation group {agno}, and the filesystem has {}",
            sb.agcount
        )));
    }
    if isize != u32::from(sb.inodesize) {
        return Err(Error::CorruptLog(format!(
            "an icreate item makes inodes of {isize} bytes on a filesystem whose inodes \
             are {}",
            sb.inodesize
        )));
    }
    if u64::from(count) * u64::from(isize) != u64::from(length) * u64::from(sb.blocksize) {
        return Err(Error::CorruptLog(format!(
            "an icreate item makes {count} inodes of {isize} bytes in {length} blocks, \
             which do not fill it"
        )));
    }

    let fsblock = (u64::from(agno) << sb.agblklog) | u64::from(agbno);
    let base = sb.fsblock_offset(fsblock);
    for slot in 0..count {
        let ino = sb.join_ino(agno, (agbno << sb.inopblog) + slot);
        let mut inode = vec![0u8; usize::from(sb.inodesize)];
        use crate::inode::offsets as at;
        inode[at::MAGIC..at::MAGIC + 2]
            .copy_from_slice(&crate::inode::XFS_DINODE_MAGIC.to_be_bytes());
        inode[at::VERSION] = 3;
        inode[at::GEN..at::GEN + 4].copy_from_slice(&gen.to_be_bytes());
        inode[at::NEXT_UNLINKED..at::NEXT_UNLINKED + 4]
            .copy_from_slice(&log_dinode::DI_NEXT_UNLINKED_NULL.to_be_bytes());
        inode[at::INO..at::INO + 8].copy_from_slice(&ino.to_be_bytes());
        inode[at::UUID..at::UUID + 16].copy_from_slice(&sb.meta_uuid);
        stamp_inode(sb, &mut inode);
        into.wrote(base + u64::from(slot) * u64::from(isize), &inode);
    }
    Ok(())
}

/// Give a replayed inode the checksum a reader will check.
fn stamp_inode(sb: &Superblock, inode: &mut [u8]) {
    if !sb.is_v5() {
        return;
    }
    let at = crate::inode::offsets::CRC;
    if inode.len() < at + 4 {
        return;
    }
    let crc = crate::superblock::crc32c_with_zeroed_crc(inode, at);
    inode[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Give a replayed buffer the checksum a reader will check.
///
/// A logged buffer carries whatever checksum it had when the kernel
/// copied it into the record, which is the one from *before* the change.
/// The kernel recomputes it as it writes the buffer out, through the
/// verifier its type names, and this does the same — otherwise every
/// replayed block is refused by this driver's own readers.
///
/// A type whose checksum is not known here is left as it was logged. A
/// reader that verifies it will refuse it and say so, which is the
/// failure that says another type belongs in this list; a reader that
/// does not verify it is unaffected either way.
fn stamp(sb: &Superblock, buf_type: u16, buffer: &mut [u8]) {
    use buf_log_format::buf_type::*;

    if !sb.is_v5() {
        return;
    }
    let at = match buf_type {
        BLFT_AGF => crate::ag::offsets::agf::CRC,
        BLFT_AGI => crate::ag::offsets::agi::CRC,
        BLFT_AGFL => crate::agfl::offsets::CRC,
        BLFT_BTREE => {
            // A short-form btree block — the free-space, inode and
            // reference-count trees — puts its checksum at 52, and the
            // block map's long-form one puts it at 64. The magic says
            // which this is.
            match btree_is_long_form(buffer) {
                true => crate::bmbt::offsets::CRC,
                false => crate::ag_btree::offsets::CRC,
            }
        }
        BLFT_DINO => {
            let inodesize = usize::from(sb.inodesize);
            for slot in 0..buffer.len() / inodesize.max(1) {
                let from = slot * inodesize;
                stamp_inode(sb, &mut buffer[from..from + inodesize]);
            }
            return;
        }
        BLFT_DIR_BLOCK | BLFT_DIR_DATA | BLFT_DIR_FREE => {
            crate::format::dir::offsets::dir3_blk::CRC
        }
        BLFT_DIR_LEAF1 | BLFT_DIR_LEAFN | BLFT_DA_NODE | BLFT_ATTR_LEAF => {
            crate::format::dir::offsets::da_blk::CRC
        }
        BLFT_SYMLINK => crate::format::symlink::offsets::CRC,
        BLFT_SB => crate::superblock::SB_CRC_OFFSET,
        _ => return,
    };
    if buffer.len() < at + 4 {
        return;
    }
    let crc = crate::superblock::crc32c_with_zeroed_crc(buffer, at);
    buffer[at..at + 4].copy_from_slice(&crc.to_le_bytes());
}

/// Whether a btree block is one of the long-form ones — a block map,
/// whose sibling pointers are 64-bit and whose header is longer.
fn btree_is_long_form(buffer: &[u8]) -> bool {
    if buffer.len() < 4 {
        return false;
    }
    let magic = crate::endian::be32(buffer, 0);
    magic == crate::bmbt::XFS_BMAP_CRC_MAGIC || magic == crate::bmbt::XFS_BMAP_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use buf_log_format::offsets as b;
    use std::sync::{Arc, Mutex};

    /// A device of zeroes, so an item's effect is whatever it wrote.
    struct MemDev(Mutex<Vec<u8>>);

    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let bytes = self.0.lock().unwrap();
            let at = offset as usize;
            buf.copy_from_slice(&bytes[at..at + buf.len()]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
    }

    /// The smallest superblock that answers what a replay asks of one:
    /// the inode size, the block size and whether checksums apply.
    fn sb() -> Superblock {
        let mut raw = vec![0u8; 512];
        raw[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        raw[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
        raw[8..16].copy_from_slice(&4096u64.to_be_bytes()); // dblocks
        raw[48..56].copy_from_slice(&4u64.to_be_bytes()); // logstart
        raw[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        raw[84..88].copy_from_slice(&1024u32.to_be_bytes()); // agblocks
        raw[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
        raw[96..100].copy_from_slice(&16u32.to_be_bytes()); // logblocks
        raw[100..102]
            .copy_from_slice(&(5u16 | crate::superblock::version_flags::MOREBITSBIT).to_be_bytes());
        raw[102..104].copy_from_slice(&512u16.to_be_bytes()); // sectsize
        raw[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        raw[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
        raw[120] = 12; // blocklog
        raw[121] = 9; // sectlog
        raw[122] = 9; // inodelog
        raw[123] = 3; // inopblog
        raw[124] = 10; // agblklog
        let crc = crate::superblock::crc32c_with_zeroed_crc(&raw, 224);
        raw[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&raw).expect("superblock")
    }

    /// A buffer item's format operation: one bitmap word, the bits given.
    fn buf_format(blkno: u64, len_bb: u16, flags: u16, map: u32) -> Vec<u8> {
        let mut f = vec![0u8; buf_log_format::BLF_HEADER_SIZE + 4];
        f[b::TYPE..b::TYPE + 2].copy_from_slice(&item_types::XFS_LI_BUF.to_ne_bytes());
        f[b::SIZE..b::SIZE + 2].copy_from_slice(&2u16.to_ne_bytes());
        f[b::FLAGS..b::FLAGS + 2].copy_from_slice(&flags.to_ne_bytes());
        f[b::LEN..b::LEN + 2].copy_from_slice(&len_bb.to_ne_bytes());
        f[b::BLKNO..b::BLKNO + 8].copy_from_slice(&blkno.to_ne_bytes());
        f[b::MAP_SIZE..b::MAP_SIZE + 4].copy_from_slice(&1u32.to_ne_bytes());
        f[b::DATA_MAP..b::DATA_MAP + 4].copy_from_slice(&map.to_ne_bytes());
        f
    }

    fn overlay(bytes: usize) -> Overlay {
        Overlay::new(Arc::new(MemDev(Mutex::new(vec![0u8; bytes]))) as Arc<dyn BlockRead>)
    }

    fn read(into: &Overlay, at: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        into.read_at(at, &mut out).expect("a read");
        out
    }

    #[test]
    fn a_bitmaps_runs_are_the_items_regions() {
        // Chunks 0..2 and 5, in a buffer of eight chunks.
        assert_eq!(runs_of(&[0b0010_0111], 8), vec![(0, 3), (5, 1)]);
        // A bit past the buffer's own length is not a run: the map is a
        // whole number of words and the tail of the last one is padding.
        assert_eq!(runs_of(&[0xFFFF_FFFF], 4), vec![(0, 4)]);
        assert!(runs_of(&[0], 8).is_empty());
    }

    /// An inode cluster's buffer item gives up only the unlinked
    /// pointer, not the stale inode image around it.
    ///
    /// The chunk holding `di_next_unlinked` holds the first 128 bytes of
    /// the inode as well — magic, mode, size, everything — as they read
    /// when the buffer was last brought in. Writing the chunk whole puts
    /// that back over whatever the inode item beside it has just
    /// written, which is how a replayed inode comes out as an older
    /// version of itself.
    #[test]
    fn an_inode_buffers_item_moves_only_the_unlinked_pointer() {
        const NEXT_UNLINKED: usize = crate::format::log_items::log_dinode::offsets::NEXT_UNLINKED;
        let sb = sb();
        let into = overlay(64 * 1024);
        let at = 8 * BBSIZE as u64;

        // The inode as it stands: a file of 4096 bytes, not on the list.
        let mut inode = vec![0u8; 512];
        inode[0..2].copy_from_slice(&crate::inode::XFS_DINODE_MAGIC.to_be_bytes());
        inode[crate::inode::offsets::VERSION] = 3;
        inode[56..64].copy_from_slice(&4096u64.to_be_bytes()); // di_size
        inode[NEXT_UNLINKED..NEXT_UNLINKED + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        into.wrote(at, &inode);

        // The logged chunk: an older image of the same inode — size
        // zero, as it was when the file was made — with the unlinked
        // pointer now naming inode 33.
        let mut chunk = vec![0u8; buf_log_format::BLF_CHUNK];
        chunk[0..2].copy_from_slice(&crate::inode::XFS_DINODE_MAGIC.to_be_bytes());
        chunk[crate::inode::offsets::VERSION] = 3;
        chunk[NEXT_UNLINKED..NEXT_UNLINKED + 4].copy_from_slice(&33u32.to_be_bytes());

        let item = Item {
            regions: vec![
                buf_format(8, 1, buf_log_format::flags::BLF_INODE_BUF, 0b1),
                chunk,
            ],
            total: 2,
        };
        let mut cancelled = HashMap::new();
        assert!(apply_buffer(&sb, &into, &item, &mut cancelled).expect("applying"));

        let after = read(&into, at, 512);
        assert_eq!(
            u32::from_be_bytes(after[NEXT_UNLINKED..NEXT_UNLINKED + 4].try_into().unwrap()),
            33,
            "the unlinked pointer is what the item was for"
        );
        assert_eq!(
            u64::from_be_bytes(after[56..64].try_into().unwrap()),
            4096,
            "and the rest of the inode is still what it was, not the stale copy the \
             chunk carried"
        );
    }

    /// A cancelled block's earlier items are not applied, and the ones
    /// after the cancellation are.
    ///
    /// A block that is freed and handed out again is a different thing
    /// afterwards. The item before the hand-over describes what it used
    /// to be, and applying it writes a directory block over an inode
    /// chunk, or over file data the log does not carry at all.
    #[test]
    fn a_cancelled_blocks_earlier_items_are_left_alone() {
        let sb = sb();
        let into = overlay(64 * 1024);
        let at = 8 * BBSIZE as u64;
        let stale = Item {
            regions: vec![
                buf_format(8, 1, 0, 0b1),
                vec![0xAA; buf_log_format::BLF_CHUNK],
            ],
            total: 2,
        };
        let cancel = Item {
            regions: vec![buf_format(8, 1, buf_log_format::flags::BLF_CANCEL, 0)],
            total: 1,
        };
        let fresh = Item {
            regions: vec![
                buf_format(8, 1, 0, 0b1),
                vec![0xBB; buf_log_format::BLF_CHUNK],
            ],
            total: 2,
        };

        let mut cancelled = HashMap::from([((8u64, 1u32), 1usize)]);
        assert!(
            !apply_buffer(&sb, &into, &stale, &mut cancelled).expect("the stale item"),
            "an item for a block that is cancelled later must not be applied"
        );
        assert_eq!(read(&into, at, 4), vec![0, 0, 0, 0]);

        assert!(!apply_buffer(&sb, &into, &cancel, &mut cancelled).expect("the cancel"));
        assert!(
            cancelled.is_empty(),
            "the cancellation is consumed by the item that placed it, so what comes \
             after it is the block's new life"
        );

        assert!(apply_buffer(&sb, &into, &fresh, &mut cancelled).expect("the fresh item"));
        assert_eq!(read(&into, at, 4), vec![0xBB; 4]);
    }
}
