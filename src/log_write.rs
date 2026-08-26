//! Building log records.
//!
//! XFS records a metadata change in the log before applying it, so that
//! a change spanning several structures either happens completely or not
//! at all. Everything this driver cannot yet do — allocation, directory
//! changes, create, unlink, rename — is blocked on being able to write
//! one of these correctly.
//!
//! # What a transaction looks like
//!
//! A record's payload is a sequence of operations, each with a 12-byte
//! header. A transaction opens with an empty operation flagged
//! `XLOG_START_TRANS`, carries a transaction header and then one or more
//! items, and closes with an empty operation flagged
//! `XLOG_COMMIT_TRANS`. An inode-core change is:
//!
//! ```text
//! op  len   what
//!  0    0   START
//!  1   16   transaction header
//!  2   56   xfs_inode_log_format — which inode, which fields
//!  3  176   the inode core, as the log stores it
//!  4    0   COMMIT
//! ```
//!
//! # Two things that are easy to get wrong
//!
//! **The logged inode is native-endian.** The on-disk inode is
//! big-endian throughout; the copy in the log is not. It is the same
//! 176-byte shape with the same field offsets, byte-swapped. Writing the
//! on-disk form into a record produces something that parses and is
//! entirely wrong.
//!
//! **The cycle stamp is applied last.** Each 512-byte block of the
//! payload has its first four bytes replaced by the cycle number, and
//! the displaced word kept in the header's `h_cycle_data`. The checksum
//! is computed over the stamped form, so the order is: build, stamp,
//! then checksum.
//!
//! # Why a mistake here is survivable
//!
//! A record whose checksum does not verify is treated by the kernel as a
//! torn write: it truncates the log head back and discards that record
//! and everything after it. So a wrong encoding does not corrupt a
//! filesystem — the transaction simply never happened. That is what
//! makes this safe to develop incrementally, and it is why the encoder
//! is checked against records the kernel wrote rather than against its
//! own output.

use crate::log::{record_checksum, BBSIZE, XLOG_HEADER_MAGIC, XLOG_REC_HEADER_SIZE};

/// `xlog_op_header` — the 12 bytes in front of every operation.
pub const OP_HEADER_SIZE: usize = 12;

/// `XFS_TRANSACTION` — the client id every filesystem transaction uses.
pub const XFS_TRANSACTION: u8 = 0x69;

/// `XLOG_START_TRANS`.
pub const XLOG_START_TRANS: u8 = 0x01;
/// `XLOG_COMMIT_TRANS`.
pub const XLOG_COMMIT_TRANS: u8 = 0x02;

/// `XFS_TRANS_HEADER_MAGIC`, stored little-endian — "TRAN".
pub const XFS_TRANS_HEADER_MAGIC: u32 = 0x5452_414e;
/// Size of `xfs_trans_header`.
pub const TRANS_HEADER_SIZE: usize = 16;

/// `XFS_LI_INODE` — the log item type for an inode.
pub const XFS_LI_INODE: u16 = 0x123b;
/// Size of `xfs_inode_log_format`.
pub const INODE_LOG_FORMAT_SIZE: usize = 56;
/// `XFS_ILOG_CORE` — the item logs the inode core.
pub const XFS_ILOG_CORE: u32 = 0x01;
/// Size of the inode core as the log stores it.
pub const LOG_DINODE_SIZE: usize = 176;

/// `XLOG_VERSION_2`.
const XLOG_VERSION_2: u32 = 2;
/// `XLOG_FMT_LINUX_LE`.
const XLOG_FMT_LINUX_LE: u32 = 1;

/// One operation: its flags and its payload.
pub struct Op {
    pub flags: u8,
    pub data: Vec<u8>,
}

/// The transaction header that opens every transaction's items.
///
/// `num_items` counts log items, not operations — an inode item is one
/// item spanning two operations.
pub fn trans_header(tid: u32, kind: u32, num_items: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(TRANS_HEADER_SIZE);
    v.extend_from_slice(&XFS_TRANS_HEADER_MAGIC.to_le_bytes());
    v.extend_from_slice(&kind.to_le_bytes());
    v.extend_from_slice(&tid.to_le_bytes());
    v.extend_from_slice(&num_items.to_le_bytes());
    v
}

/// `xfs_inode_log_format` — which inode is being logged, and which parts.
pub fn inode_log_format(ino: u64, fields: u32) -> Vec<u8> {
    let mut v = vec![0u8; INODE_LOG_FORMAT_SIZE];
    v[0..2].copy_from_slice(&XFS_LI_INODE.to_le_bytes());
    // The item spans two operations: this format, then the core.
    v[2..4].copy_from_slice(&2u16.to_le_bytes());
    v[4..8].copy_from_slice(&fields.to_le_bytes());
    v[16..24].copy_from_slice(&ino.to_le_bytes());
    v
}

/// Assemble a record's payload from its operations.
///
/// Every operation carries the same transaction id; that is what ties
/// them together across a record boundary.
pub fn payload(tid: u32, ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    for op in ops {
        out.extend_from_slice(&tid.to_be_bytes());
        out.extend_from_slice(&(op.data.len() as u32).to_be_bytes());
        out.push(XFS_TRANSACTION);
        out.push(op.flags);
        out.extend_from_slice(&[0, 0]); // oh_res2
        out.extend_from_slice(&op.data);
    }
    out
}

/// Where a record is going, and what the log looked like before it.
pub struct Placement {
    /// Basic block within the log at which the record starts.
    pub block: u32,
    /// Cycle this record belongs to.
    pub cycle: u32,
    /// Start block of the previous record, or `u32::MAX` for none.
    pub prev_block: u32,
    /// Oldest record still needed, as a log sequence number.
    pub tail_lsn: u64,
    /// The filesystem's UUID, which every record carries.
    pub uuid: [u8; 16],
    /// The log's iclog size, from an existing record's `h_size`.
    pub iclog_size: u32,
}

/// Encode one complete record: header block, then the stamped payload.
///
/// Returns the bytes to write at `placement.block`, padded to whole
/// basic blocks. The checksum is computed last, over the header struct
/// and the stamped payload, because that is what the kernel verifies.
pub fn encode_record(placement: &Placement, num_logops: u32, payload: &[u8]) -> Vec<u8> {
    let padded = payload.len().div_ceil(BBSIZE) * BBSIZE;
    let mut data = vec![0u8; padded];
    data[..payload.len()].copy_from_slice(payload);

    let mut header = vec![0u8; BBSIZE];
    header[0..4].copy_from_slice(&XLOG_HEADER_MAGIC.to_be_bytes());
    header[4..8].copy_from_slice(&placement.cycle.to_be_bytes());
    header[8..12].copy_from_slice(&XLOG_VERSION_2.to_be_bytes());
    header[12..16].copy_from_slice(&(payload.len() as u32).to_be_bytes());
    let lsn = (u64::from(placement.cycle) << 32) | u64::from(placement.block);
    header[16..24].copy_from_slice(&lsn.to_be_bytes());
    header[24..32].copy_from_slice(&placement.tail_lsn.to_be_bytes());
    header[36..40].copy_from_slice(&placement.prev_block.to_be_bytes());
    header[40..44].copy_from_slice(&num_logops.to_be_bytes());
    header[300..304].copy_from_slice(&XLOG_FMT_LINUX_LE.to_be_bytes());
    header[304..320].copy_from_slice(&placement.uuid);
    header[320..324].copy_from_slice(&placement.iclog_size.to_be_bytes());

    // Stamp each payload block's first word with the cycle, keeping the
    // displaced word in the header. A reader undoes this to recover the
    // payload; the checksum covers the stamped form.
    let blocks = padded / BBSIZE;
    for k in 0..blocks.min(64) {
        let at = k * BBSIZE;
        let original: [u8; 4] = data[at..at + 4].try_into().expect("4 bytes");
        header[44 + k * 4..48 + k * 4].copy_from_slice(&original);
        data[at..at + 4].copy_from_slice(&placement.cycle.to_be_bytes());
    }

    let crc = record_checksum(&header, &data[..payload.len()]);
    header[32..36].copy_from_slice(&crc.to_le_bytes());

    let mut out = header;
    out.extend_from_slice(&data);
    debug_assert!(out.len() >= XLOG_REC_HEADER_SIZE);
    out
}

/// Convert an inode's on-disk bytes into the form the log stores.
///
/// The two are the same structure at the same offsets. They differ in
/// exactly two ways, and both are easy to get wrong in a way that parses
/// cleanly and is entirely incorrect:
///
/// - **Byte order.** The on-disk inode is big-endian throughout. The log
///   copy is **native**-endian — the published format document says so
///   outright: *"All on-disk values are in big-endian format except the
///   journaling log which is in native endian format."* So this is a
///   swap on a little-endian host and a copy on a big-endian one, which
///   is why it goes through `to_native` rather than reversing
///   unconditionally. Only the UUID and the padding are carried across
///   untouched in either case.
/// - **The checksum is blank.** `di_crc` is a real value on disk and
///   always zero in the log. Whatever replays the record recomputes it,
///   so copying the disk value across would be wrong even though nothing
///   would complain.
///
/// A version-2 inode logs a **96-byte** core rather than 176, so the
/// length is taken from `di_version` rather than assumed.
///
/// Two features move fields, and both are read from `di_flags2` in the
/// inode itself rather than from the superblock, because it is the
/// inode's own encoding that matters:
///
/// - `bigtime` stores each timestamp as one 64-bit count; without it
///   they are a pair of 32-bit halves, which swap differently.
/// - `nrext64` moves the data-extent count to offset 24 as a 64-bit
///   field and the attribute count to 76, leaving 80 as padding.
pub fn log_dinode_from_disk(raw: &[u8]) -> Result<Vec<u8>, &'static str> {
    if raw.len() < V2_LOG_DINODE_SIZE {
        return Err("inode is shorter than a v2 core");
    }
    let version = raw[offsets::VERSION];
    let size = match version {
        1 | 2 => V2_LOG_DINODE_SIZE,
        3 => LOG_DINODE_SIZE,
        _ => return Err("unrecognised inode version"),
    };
    if raw.len() < size {
        return Err("inode is shorter than its version's core");
    }

    let flags2 = if version >= 3 {
        u64::from_be_bytes(
            raw[offsets::FLAGS2..offsets::FLAGS2 + 8]
                .try_into()
                .unwrap(),
        )
    } else {
        0
    };
    let bigtime = flags2 & DI_FLAGS2_BIGTIME != 0;
    let nrext64 = flags2 & DI_FLAGS2_NREXT64 != 0;

    let mut out = raw[..size].to_vec();
    if cfg!(target_endian = "little") {
        // The disk is big-endian and the log is native, so on a
        // little-endian host every multi-byte field is reversed. On a
        // big-endian host the two agree and the copy stands as it is.
        for &(at, width) in field_layout(version, bigtime, nrext64) {
            if at + width > size {
                continue;
            }
            out[at..at + width].reverse();
        }
    }
    if version >= 3 {
        out[offsets::CRC..offsets::CRC + 4].copy_from_slice(&[0, 0, 0, 0]);
    }
    Ok(out)
}

/// Size of the core a version-1 or -2 inode logs.
pub const V2_LOG_DINODE_SIZE: usize = 96;

/// `XFS_DIFLAG2_BIGTIME`.
const DI_FLAGS2_BIGTIME: u64 = 0x08;
/// `XFS_DIFLAG2_NREXT64`.
const DI_FLAGS2_NREXT64: u64 = 0x10;

/// Offsets within the inode core, shared by the on-disk and logged forms.
mod offsets {
    pub const VERSION: usize = 4;
    pub const CRC: usize = 100;
    pub const FLAGS2: usize = 120;
}

/// Every multi-byte field, as `(offset, width)`.
///
/// Padding and the UUID are absent deliberately: they are the only spans
/// carried across without swapping, so leaving them out of the table is
/// what makes them stay put.
fn field_layout(version: u8, bigtime: bool, nrext64: bool) -> &'static [(usize, usize)] {
    // The core up to offset 96, which is all a v2 inode has.
    const COMMON: &[(usize, usize)] = &[
        (0, 2),  // di_magic
        (2, 2),  // di_mode
        (6, 2),  // unused
        (8, 4),  // di_uid
        (12, 4), // di_gid
        (16, 4), // di_nlink
        (20, 2), // di_projid_lo
        (22, 2), // di_projid_hi
        (56, 8), // di_size
        (64, 8), // di_nblocks
        (72, 4), // di_extsize
        (84, 4), // di_dmevmask
        (88, 2), // di_dmstate
        (90, 2), // di_flags
        (92, 4), // di_gen
    ];
    // v3 adds everything past di_next_unlinked.
    const V3_TAIL: &[(usize, usize)] = &[
        (96, 4),  // di_next_unlinked
        (104, 8), // di_changecount
        (112, 8), // di_lsn
        (120, 8), // di_flags2
        (128, 4), // di_cowextsize
        (152, 8), // di_ino
    ];

    // Built once per shape rather than per call. The combinations are
    // few and fixed, so the alternative is allocating on every inode.
    macro_rules! shape {
        ($name:ident, $extra:expr) => {{
            static TABLE: std::sync::OnceLock<Vec<(usize, usize)>> = std::sync::OnceLock::new();
            TABLE
                .get_or_init(|| {
                    let mut v = COMMON.to_vec();
                    v.extend_from_slice($extra);
                    v
                })
                .as_slice()
        }};
    }

    // Timestamps: one 64-bit count under bigtime, two 32-bit halves
    // otherwise. They swap differently, so the shape has to know.
    let times: &[(usize, usize)] = if bigtime {
        &[(32, 8), (40, 8), (48, 8), (144, 8)]
    } else {
        &[
            (32, 4),
            (36, 4),
            (40, 4),
            (44, 4),
            (48, 4),
            (52, 4),
            (144, 4),
            (148, 4),
        ]
    };
    // Extent counts move under nrext64.
    let counts: &[(usize, usize)] = if nrext64 {
        &[(24, 8), (76, 4)]
    } else {
        &[(76, 4), (80, 2)]
    };

    match (version >= 3, bigtime, nrext64) {
        (false, _, _) => {
            // A v2 core stops at 96 and has no bigtime or nrext64.
            shape!(
                v2,
                &[
                    (32, 4),
                    (36, 4),
                    (40, 4),
                    (44, 4),
                    (48, 4),
                    (52, 4),
                    (76, 4),
                    (80, 2)
                ]
            )
        }
        (true, true, false) => shape!(v3_bt, &[V3_TAIL, times, counts].concat()),
        (true, true, true) => shape!(v3_bt_nr, &[V3_TAIL, times, counts].concat()),
        (true, false, false) => shape!(v3_lt, &[V3_TAIL, times, counts].concat()),
        (true, false, true) => shape!(v3_lt_nr, &[V3_TAIL, times, counts].concat()),
    }
}
