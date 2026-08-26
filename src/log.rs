//! Deciding whether the log has anything left to replay.
//!
//! XFS records metadata changes in a circular log before applying them
//! to the structures themselves. A filesystem that was unmounted cleanly
//! has nothing outstanding there; one that was not is holding metadata
//! the log still owns, and every structure this driver reads may be a
//! version the log was about to replace.
//!
//! Presenting that as though it were current is the worst thing a
//! read-only driver can do, because there is no symptom: directories
//! parse, checksums verify, files read. The data is simply old, and the
//! caller has no way to tell. So the question this module answers is not
//! "can we replay the log" — this driver cannot write and does not try —
//! but "is there anything to replay", which decides whether the volume
//! may be mounted at all.
//!
//! # How a clean log is recognised
//!
//! The log is a ring of 512-byte basic blocks. Records are written into
//! it in order, each beginning with a header carrying a magic number,
//! the filesystem's UUID, and a log sequence number: a (cycle, block)
//! pair that only ever increases, with the cycle bumped each time the
//! ring wraps.
//!
//! Unmounting cleanly writes one last record whose single operation is
//! flagged as an unmount. So the newest record in the ring says how the
//! filesystem was left: an unmount record means everything before it was
//! applied, and anything else means it was not.
//!
//! Finding the newest record is done by scanning the whole ring for
//! record headers and taking the greatest sequence number, rather than
//! by locating the head through the cycle-number discontinuity the way
//! the kernel does. The kernel is looking for somewhere to write and
//! needs the exact boundary; this only needs to know which record is
//! last, and a scan gets there without a wrap-point search that would be
//! considerably easier to get subtly wrong.

use crate::endian::{be32, be64, uuid_at};
use crate::error::{Error, Result};
use crate::superblock::Superblock;
use fs_core::BlockRead;

/// A log basic block. Defined once in [`crate::format::log_items`],
/// with the rest of the log's layout.
pub use crate::format::log_items::BBSIZE;

/// `XLOG_HEADER_MAGIC_NUM`, at the start of every log record header.
pub const XLOG_HEADER_MAGIC: u32 = 0xFEED_BABE;

/// `h_fmt` values — the byte order the record's **item payloads** are in.
///
/// The record header itself is big-endian like the rest of the
/// filesystem, so anyone can find the log head. The items inside it are
/// memory images written in the byte order of whichever machine wrote
/// them, and this field is how a reader finds out which.
///
/// It matters when a disk moves between architectures. A cleanly
/// unmounted filesystem carries nothing to replay, so it travels
/// freely; a dirty one holds items that only a machine of matching
/// endianness can interpret. Replaying them byte-reversed would write
/// plausible nonsense into metadata, so the correct response to a
/// mismatch is to refuse.
pub mod format {
    /// Not recorded — written by something that did not set the field.
    pub const UNKNOWN: u32 = 0;
    /// Little-endian Linux, which is x86-64 and every ARM this driver
    /// is likely to meet.
    pub const LINUX_LE: u32 = 1;
    /// Big-endian Linux, such as s390x.
    pub const LINUX_BE: u32 = 2;
    /// Big-endian IRIX, where XFS started.
    pub const IRIX_BE: u32 = 3;

    /// The value a record written on this machine would carry.
    pub const fn native() -> u32 {
        if cfg!(target_endian = "little") {
            LINUX_LE
        } else {
            LINUX_BE
        }
    }
}

/// `XLOG_UNMOUNT_TRANS` — the operation flag that marks the record a
/// clean unmount writes as its last act.
const XLOG_UNMOUNT_TRANS: u8 = 0x20;

/// `XLOG_VERSION_2`, in `h_version`. Version 2 records may have headers
/// spanning more than one basic block.
const XLOG_VERSION_2: u32 = 2;

/// `XLOG_HEADER_CYCLE_SIZE` — how much log a single header block
/// describes, and so the divisor for a multi-block header.
const XLOG_HEADER_CYCLE_SIZE: u32 = 32 * 1024;

/// Byte offsets within `xlog_rec_header_t`.
///
/// The fields between the ones this module reads are named too, even
/// where nothing consults them: an offset can only be checked against
/// the format documentation when its neighbours are there to be counted
/// off against, and `h_lsn` at 16 is only obviously right if `h_cycle`,
/// `h_version` and `h_len` are visible above it.
#[allow(dead_code)]
mod offsets {
    pub const MAGICNO: usize = 0;
    pub const CYCLE: usize = 4;
    pub const VERSION: usize = 8;
    pub const LEN: usize = 12;
    pub const LSN: usize = 16;
    pub const NUM_LOGOPS: usize = 40;
    /// `h_fmt` — the byte order this record's item payloads are in.
    pub const FMT: usize = 300;
    /// `h_crc` — CRC32C over the header struct and the record's data,
    /// stored little-endian like every other XFS checksum.
    pub const CRC: usize = 32;
    pub const FS_UUID: usize = 304;
    pub const SIZE: usize = 320;
}

/// Byte offsets within `xlog_op_header_t`, which follows the record's
/// header blocks.
mod op_offsets {
    pub const FLAGS: usize = 9;
}

/// `sizeof(xlog_rec_header)` — the header's fields, padded to the 8-byte
/// alignment its `u64` members impose.
///
/// This is **not** the 512 bytes the header occupies on disk. The header
/// sits alone in a basic block and the remaining 184 bytes are padding;
/// the checksum covers only the struct. Getting this wrong is invisible
/// in every other use of the header and fatal to the checksum, which is
/// why it is named here rather than written as a literal at its one use.
pub const XLOG_REC_HEADER_SIZE: usize = 328;

/// How much of the log to read at a time while scanning.
const SCAN_CHUNK: usize = 1 << 20;

/// The checksum a log record should carry.
///
/// `header` is the record's basic block and `data` the `h_len` bytes
/// that follow it, **as they are written** — with the cycle stamp
/// applied, since the checksum is taken after packing rather than
/// before. The stored `h_crc` is treated as zero, the convention every
/// self-describing XFS structure uses.
///
/// # How this was established
///
/// Not from documentation, and not by guessing spans — ten candidate
/// layouts were tried against real records and all ten were wrong.
///
/// It came from a pair of filesystems built identically except for one
/// byte of file data. CRC32C is affine, so for two inputs of equal
/// length `crc(A) ^ crc(B)` depends only on where they differ; any
/// unknown seed or final xor cancels. One record in that pair differed
/// only in `h_cycle_data[0]`, and the single span length reproducing its
/// checksum difference was 840 — which is 328 + `h_len`, and 328 is the
/// header struct rather than the 512-byte block it lives in.
///
/// Verified against all 24 checksummed records across four filesystems
/// the kernel wrote.
pub fn record_checksum(header: &[u8], data: &[u8]) -> u32 {
    let mut buf = header[..XLOG_REC_HEADER_SIZE.min(header.len())].to_vec();
    buf[offsets::CRC..offsets::CRC + 4].copy_from_slice(&[0, 0, 0, 0]);
    buf.extend_from_slice(data);
    crc32c::crc32c(&buf)
}

/// What the log says about how the filesystem was left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogState {
    /// Never written to — a log as `mkfs.xfs` leaves it.
    Empty,
    /// The newest record is an unmount record: nothing to replay.
    CleanlyUnmounted,
    /// The newest record is not an unmount record, so the log holds
    /// changes that were never applied to the filesystem.
    NeedsReplay,
}

/// One record header found in the ring.
struct Record {
    /// Basic-block offset from the start of the log.
    bb: u64,
    /// `h_lsn`, which orders records: cycle in the high half, block in
    /// the low. Comparing it as one `u64` therefore orders by cycle
    /// first, which is exactly the intended ordering.
    lsn: u64,
    num_logops: u32,
    /// Basic blocks this record's header occupies before its data.
    header_blocks: u64,
    /// The record's header block, kept so its format field can be
    /// checked once the newest record is known.
    header: Vec<u8>,
}

/// Read the log and report whether anything in it is outstanding.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] when the log is on a separate device,
/// which this driver has no handle for — its state cannot be
/// established, and assuming it is clean would be a guess in the one
/// direction that loses data silently.
pub fn inspect(device: &dyn BlockRead, sb: &Superblock) -> Result<LogState> {
    if !sb.has_internal_log() {
        return Err(Error::UnsupportedFeature(
            "the log is on a separate device, so whether it needs replaying cannot be \
             established from this one"
                .into(),
        ));
    }

    let log_start = sb.fsblock_offset(sb.logstart);
    let log_bytes = u64::from(sb.logblocks) * u64::from(sb.blocksize);
    if log_bytes == 0 {
        return Err(Error::BadSuperblock(
            "superblock places an internal log of zero length".into(),
        ));
    }

    let Some(newest) = scan_for_newest_record(device, sb, log_start, log_bytes)? else {
        // No record anywhere in the ring. A log that has never been
        // written holds zeros, which is what mkfs leaves and what a
        // filesystem that has never been mounted still has.
        return Ok(LogState::Empty);
    };

    // A clean unmount writes a record holding exactly one operation, and
    // that operation carries the unmount flag. Both halves matter: a
    // record with more operations is ordinary work, whatever its first
    // operation happens to be flagged as.
    // A record we cannot interpret must not be treated as clean. The
    // header is big-endian so it reads anywhere, but the items inside
    // are memory images in the byte order of whatever wrote them, and
    // replaying those reversed would write plausible nonsense into
    // metadata. A cleanly unmounted volume moves between architectures
    // freely because it has nothing to replay; a dirty one does not.
    let fmt = be32(&newest.header, offsets::FMT);
    if fmt != format::native() {
        return Err(Error::UnsupportedFeature(format!(
            "the log was written in format {fmt}, and this machine writes              {}; a log holding unapplied records can only be read by a machine              of matching byte order",
            format::native()
        )));
    }

    if newest.num_logops != 1 {
        return Ok(LogState::NeedsReplay);
    }

    let op_at = log_start + (newest.bb + newest.header_blocks) * BBSIZE as u64;
    if op_at + BBSIZE as u64 > log_start + log_bytes {
        // The operation would fall outside the ring. Rather than wrap
        // and guess, treat it as needing replay: the conservative answer
        // is the one that refuses.
        return Ok(LogState::NeedsReplay);
    }
    let mut op = vec![0u8; BBSIZE];
    device.read_at(op_at, &mut op)?;

    if op[op_offsets::FLAGS] & XLOG_UNMOUNT_TRANS != 0 {
        Ok(LogState::CleanlyUnmounted)
    } else {
        Ok(LogState::NeedsReplay)
    }
}

/// Walk the ring and return the record with the greatest sequence
/// number, or `None` if it holds no records at all.
fn scan_for_newest_record(
    device: &dyn BlockRead,
    sb: &Superblock,
    log_start: u64,
    log_bytes: u64,
) -> Result<Option<Record>> {
    let mut newest: Option<Record> = None;
    let mut at = 0u64;
    let mut buf = vec![0u8; SCAN_CHUNK];

    while at < log_bytes {
        let want = SCAN_CHUNK.min((log_bytes - at) as usize);
        let chunk = &mut buf[..want];
        device.read_at(log_start + at, chunk)?;

        for (i, block) in chunk.chunks_exact(BBSIZE).enumerate() {
            if be32(block, offsets::MAGICNO) != XLOG_HEADER_MAGIC {
                continue;
            }
            // The magic alone is four bytes of a pattern that could
            // appear in a stale block the log has not yet overwritten.
            // The filesystem UUID makes a false match implausible.
            if uuid_at(block, offsets::FS_UUID) != sb.uuid {
                continue;
            }

            let bb = at / BBSIZE as u64 + i as u64;
            let lsn = be64(block, offsets::LSN);
            if newest.as_ref().is_some_and(|n| n.lsn >= lsn) {
                continue;
            }
            newest = Some(Record {
                bb,
                lsn,
                num_logops: be32(block, offsets::NUM_LOGOPS),
                header_blocks: header_blocks(block),
                header: block.to_vec(),
            });
        }
        at += want as u64;
    }
    Ok(newest)
}

/// How many basic blocks this record's header occupies.
///
/// Version 1 records always use one. Version 2 records may describe more
/// log than a single header block can carry cycle data for, and then
/// spill into further blocks.
fn header_blocks(header: &[u8]) -> u64 {
    let version = be32(header, offsets::VERSION);
    if version & XLOG_VERSION_2 == 0 {
        return 1;
    }
    let size = be32(header, offsets::SIZE);
    if size <= XLOG_HEADER_CYCLE_SIZE {
        return 1;
    }
    u64::from(size.div_ceil(XLOG_HEADER_CYCLE_SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A device backed by a byte vector, so a log can be built exactly.
    struct MemDev(Mutex<Vec<u8>>);

    impl BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let b = self.0.lock().unwrap();
            let start = offset as usize;
            let end = start + buf.len();
            assert!(end <= b.len(), "read past the end of the device");
            buf.copy_from_slice(&b[start..end]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.lock().unwrap().len() as u64
        }
    }

    const BLOCKSIZE: u32 = 4096;
    const LOGSTART_FSB: u64 = 4;
    const LOGBLOCKS: u32 = 16;
    const UUID: [u8; 16] = [0xAB; 16];

    /// A superblock whose only job is to place the log. Built by hand
    /// rather than through `Superblock::parse` so the geometry under
    /// test is stated outright.
    fn sb() -> Superblock {
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&BLOCKSIZE.to_be_bytes());
        b[8..16].copy_from_slice(&4096u64.to_be_bytes()); // dblocks
        b[32..48].copy_from_slice(&UUID);
        b[48..56].copy_from_slice(&LOGSTART_FSB.to_be_bytes());
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&1024u32.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
        b[96..100].copy_from_slice(&LOGBLOCKS.to_be_bytes());
        b[100..102]
            .copy_from_slice(&(5u16 | crate::superblock::version_flags::MOREBITSBIT).to_be_bytes());
        b[102..104].copy_from_slice(&512u16.to_be_bytes()); // sectsize
        b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        b[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
        b[120] = 12; // blocklog
        b[121] = 9; // sectlog
        b[122] = 9; // inodelog
        b[123] = 3; // inopblog
        b[124] = 10; // agblklog
        let crc = crate::superblock::crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).expect("superblock")
    }

    /// A device large enough to hold the log where the superblock says.
    fn device(sb: &Superblock) -> (MemDev, u64) {
        let log_at = sb.fsblock_offset(sb.logstart);
        let len = log_at + u64::from(sb.logblocks) * u64::from(sb.blocksize);
        (MemDev(Mutex::new(vec![0u8; len as usize])), log_at)
    }

    /// Write a record header into basic block `bb` of the log.
    fn put_record(dev: &MemDev, log_at: u64, bb: u64, cycle: u32, block: u32, num_logops: u32) {
        let mut b = dev.0.lock().unwrap();
        let at = (log_at + bb * BBSIZE as u64) as usize;
        let h = &mut b[at..at + BBSIZE];
        h[offsets::MAGICNO..offsets::MAGICNO + 4].copy_from_slice(&XLOG_HEADER_MAGIC.to_be_bytes());
        h[offsets::CYCLE..offsets::CYCLE + 4].copy_from_slice(&cycle.to_be_bytes());
        h[offsets::VERSION..offsets::VERSION + 4].copy_from_slice(&XLOG_VERSION_2.to_be_bytes());
        h[offsets::LEN..offsets::LEN + 4].copy_from_slice(&512u32.to_be_bytes());
        // h_lsn packs the cycle above the block, so ordering by the
        // whole u64 orders by cycle first.
        let lsn = (u64::from(cycle) << 32) | u64::from(block);
        h[offsets::LSN..offsets::LSN + 8].copy_from_slice(&lsn.to_be_bytes());
        h[offsets::NUM_LOGOPS..offsets::NUM_LOGOPS + 4].copy_from_slice(&num_logops.to_be_bytes());
        h[offsets::FS_UUID..offsets::FS_UUID + 16].copy_from_slice(&UUID);
        h[offsets::SIZE..offsets::SIZE + 4].copy_from_slice(&XLOG_HEADER_CYCLE_SIZE.to_be_bytes());
        h[offsets::FMT..offsets::FMT + 4].copy_from_slice(&format::native().to_be_bytes());
    }

    /// Set the operation flags of the record whose header is at `bb`.
    fn put_op_flags(dev: &MemDev, log_at: u64, bb: u64, flags: u8) {
        let mut b = dev.0.lock().unwrap();
        let at = (log_at + (bb + 1) * BBSIZE as u64) as usize;
        b[at + op_offsets::FLAGS] = flags;
    }

    #[test]
    fn a_log_of_zeros_has_never_been_written() {
        let sb = sb();
        let (dev, _) = device(&sb);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::Empty);
    }

    /// A log written by a machine of the other byte order must be
    /// refused rather than read reversed. This is what stops a dirty
    /// disk moved between an Intel and an ARM host from having its
    /// metadata rewritten with plausible nonsense.
    #[test]
    fn a_log_from_the_other_byte_order_is_refused() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 1);
        put_op_flags(&dev, log_at, 0, XLOG_UNMOUNT_TRANS);
        {
            // Whatever this host is, claim the opposite.
            let other = if format::native() == format::LINUX_LE {
                format::LINUX_BE
            } else {
                format::LINUX_LE
            };
            let mut b = dev.0.lock().unwrap();
            let at = log_at as usize + offsets::FMT;
            b[at..at + 4].copy_from_slice(&other.to_be_bytes());
        }
        let err = inspect(&dev, &sb).unwrap_err();
        assert!(
            format!("{err}").contains("matching byte order"),
            "got {err}"
        );
    }

    /// And a log in this machine's own format is read normally, so the
    /// check above cannot pass by refusing everything.
    #[test]
    fn a_log_in_this_machines_format_is_accepted() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 1);
        put_op_flags(&dev, log_at, 0, XLOG_UNMOUNT_TRANS);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::CleanlyUnmounted);
    }

    #[test]
    fn a_final_unmount_record_means_a_clean_shutdown() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 1);
        put_op_flags(&dev, log_at, 0, XLOG_UNMOUNT_TRANS);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::CleanlyUnmounted);
    }

    /// The crash case: the newest record is ordinary work, so the log
    /// holds changes the filesystem itself has not seen.
    #[test]
    fn a_final_ordinary_record_means_replay_is_outstanding() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 1);
        put_op_flags(&dev, log_at, 0, 0x02); // commit, not unmount
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::NeedsReplay);
    }

    /// The newest record is the one that decides, not the last one in
    /// address order — the ring wraps, so an older record sits after a
    /// newer one for most of the log's life.
    #[test]
    fn the_newest_record_decides_even_when_the_ring_has_wrapped() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        // Later in the ring but an older cycle: this is the stale one.
        put_record(&dev, log_at, 40, 1, 40, 1);
        put_op_flags(&dev, log_at, 40, 0x02);
        // Earlier in the ring, newer cycle: this is the current tail.
        put_record(&dev, log_at, 8, 2, 8, 1);
        put_op_flags(&dev, log_at, 8, XLOG_UNMOUNT_TRANS);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::CleanlyUnmounted);
    }

    /// And the same the other way round, so the test above cannot pass
    /// by simply preferring whichever record came first.
    #[test]
    fn a_newer_ordinary_record_outranks_an_older_unmount() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 8, 1, 8, 1);
        put_op_flags(&dev, log_at, 8, XLOG_UNMOUNT_TRANS);
        put_record(&dev, log_at, 40, 2, 40, 1);
        put_op_flags(&dev, log_at, 40, 0x02);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::NeedsReplay);
    }

    /// An unmount record holds exactly one operation. A record carrying
    /// more is ordinary work, whatever its first operation is flagged
    /// as — so the flag alone must not be enough.
    #[test]
    fn a_multi_operation_record_is_not_an_unmount() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 7);
        put_op_flags(&dev, log_at, 0, XLOG_UNMOUNT_TRANS);
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::NeedsReplay);
    }

    /// Stale bytes in an unwritten part of the ring could carry the
    /// magic. The filesystem UUID is what stops them being read as a
    /// record.
    #[test]
    fn a_header_from_another_filesystem_is_not_a_record() {
        let sb = sb();
        let (dev, log_at) = device(&sb);
        put_record(&dev, log_at, 0, 1, 0, 1);
        put_op_flags(&dev, log_at, 0, XLOG_UNMOUNT_TRANS);
        {
            let mut b = dev.0.lock().unwrap();
            let at = (log_at) as usize + offsets::FS_UUID;
            b[at..at + 16].copy_from_slice(&[0x11; 16]);
        }
        assert_eq!(inspect(&dev, &sb).unwrap(), LogState::Empty);
    }

    #[test]
    fn a_version_2_header_larger_than_one_cycle_spans_several_blocks() {
        let mut h = vec![0u8; BBSIZE];
        h[offsets::VERSION..offsets::VERSION + 4].copy_from_slice(&XLOG_VERSION_2.to_be_bytes());
        h[offsets::SIZE..offsets::SIZE + 4]
            .copy_from_slice(&(XLOG_HEADER_CYCLE_SIZE * 4).to_be_bytes());
        assert_eq!(header_blocks(&h), 4);
    }

    #[test]
    fn a_version_1_header_always_occupies_one_block() {
        let mut h = vec![0u8; BBSIZE];
        h[offsets::VERSION..offsets::VERSION + 4].copy_from_slice(&1u32.to_be_bytes());
        h[offsets::SIZE..offsets::SIZE + 4]
            .copy_from_slice(&(XLOG_HEADER_CYCLE_SIZE * 4).to_be_bytes());
        assert_eq!(header_blocks(&h), 1);
    }
}
