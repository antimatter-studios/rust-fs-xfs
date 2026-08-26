//! Every buffer log item the kernel wrote, re-encoded and compared byte
//! for byte.
//!
//! The buffer item is the structure allocation, truncate, create, unlink
//! and every directory past shortform rest on, and almost everything
//! about it is wrong in the obvious reading: the address is in 512-byte
//! basic blocks rather than filesystem blocks, the bitmap covers
//! 128-byte chunks, the structure is little-endian inside a big-endian
//! record, and there is no padding after the bitmap.
//!
//! Unit tests can show the encoder is self-consistent. They cannot show
//! it agrees with XFS. So this takes the items out of logs the Linux
//! kernel wrote, rebuilds each one through [`BufferItem`] from nothing
//! but its address, its dirty chunks and their contents, and requires
//! the bytes to come back identical — every field, in every item, across
//! every geometry the fixtures cover.
//!
//! A disagreement here is a real disagreement with XFS. That is the
//! point: an encoder checked only against its own idea of the format
//! will encode its own misunderstanding perfectly.
//!
//! # What is deliberately not covered
//!
//! An operation too large for the remainder of a record is truncated and
//! continued in the next one. Reassembling those is a property of the
//! record framing rather than of the item, so items containing a split
//! operation are counted and skipped, and the count is reported — a
//! silent skip could hide the whole corpus going missing.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-log-fixtures.sh` and
//! `./scripts/vm-build-stress-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::buf_write::BufferItem;
use fs_xfs::format::log_items::buf_log_format::{
    flags::BLF_FLAG_MASK, offsets as at, BLF_CHUNK, BLF_HEADER_SIZE, BLF_TYPE_SHIFT,
};
use fs_xfs::log::{BBSIZE, XLOG_HEADER_MAGIC};
use fs_xfs::superblock::Superblock;
use std::path::{Path, PathBuf};

/// `XFS_LI_BUF`, the item type this test is about.
const BUF_ITEM_TYPE: u16 = 0x123c;

/// `XLOG_CONTINUE_TRANS` and `XLOG_WAS_CONT_TRANS`: an operation split
/// across a record boundary, and the remainder of one.
const OP_CONTINUES: u8 = 0x04;
const OP_WAS_CONTINUED: u8 = 0x18;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn le16(b: &[u8], i: usize) -> u16 {
    u16::from_le_bytes(b[i..i + 2].try_into().unwrap())
}
fn le32(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap())
}
fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}

/// One operation as it sits in a record.
struct RawOp {
    flags: u8,
    data: Vec<u8>,
}

/// Every operation in every committed record of an image's log, in
/// order.
///
/// `mkfs` pre-stamps the ring with headers carrying the record magic and
/// a cycle of zero, so cycle zero is skipped: trusting the magic alone
/// finds thousands of records that were never written.
fn ops_in_log(image: &Path) -> Option<Vec<RawOp>> {
    let dev = FileDevice::open(image).ok()?;
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).ok()?;
    let sb = Superblock::parse(&sbb).ok()?;

    let mut log = vec![0u8; (u64::from(sb.logblocks) * u64::from(sb.blocksize)) as usize];
    dev.read_at(sb.fsblock_offset(sb.logstart), &mut log).ok()?;

    let mut ops = Vec::new();
    for i in 0..log.len() / BBSIZE {
        let header = &log[i * BBSIZE..(i + 1) * BBSIZE];
        if be32(header, 0) != XLOG_HEADER_MAGIC || header[304..320] != sb.uuid[..] {
            continue;
        }
        let h_len = be32(header, 12) as usize;
        let logops = be32(header, 40);
        let lsn = u64::from_be_bytes(header[16..24].try_into().unwrap());
        if h_len == 0 || logops == 0 || (lsn >> 32) == 0 {
            continue;
        }

        let padded = h_len.div_ceil(BBSIZE) * BBSIZE;
        let data_at = (i + 1) * BBSIZE;
        if data_at + padded > log.len() {
            continue;
        }
        // Undo the cycle stamping: each payload block's first word was
        // replaced by the cycle number and the original kept in the
        // header.
        let mut payload = log[data_at..data_at + padded].to_vec();
        for k in 0..(padded / BBSIZE).min(64) {
            payload[k * BBSIZE..k * BBSIZE + 4].copy_from_slice(&header[44 + k * 4..48 + k * 4]);
        }
        payload.truncate(h_len);

        let mut off = 0usize;
        let mut seen = 0;
        while seen < logops && off + 12 <= payload.len() {
            let oplen = be32(&payload, off + 4) as usize;
            let flags = payload[off + 9];
            let body = off + 12;
            if body + oplen > payload.len() {
                break;
            }
            ops.push(RawOp {
                flags,
                data: payload[body..body + oplen].to_vec(),
            });
            off = body + oplen;
            seen += 1;
        }
    }
    Some(ops)
}

/// A buffer item lifted out of a log: its format operation and the data
/// operations that belong to it.
struct ParsedItem {
    format: Vec<u8>,
    data_ops: Vec<Vec<u8>>,
}

/// Does this operation look like a buffer item's format operation?
///
/// The type alone is not enough — a data operation's first two bytes can
/// hold anything, including this type. The documented size invariant
/// `op_len == 20 + 4 * map_size` is what separates them, and it is
/// checked here rather than asserted so that a coincidence is passed
/// over rather than reported as a corrupt item.
fn looks_like_format_op(op: &[u8]) -> bool {
    if op.len() < BLF_HEADER_SIZE || le16(op, at::TYPE) != BUF_ITEM_TYPE {
        return false;
    }
    let map_size = le32(op, at::MAP_SIZE) as usize;
    let size = le16(op, at::SIZE) as usize;
    op.len() == BLF_HEADER_SIZE + 4 * map_size && size >= 1
}

/// Split a run of operations into the buffer items it contains.
///
/// Consuming each item's data operations is what keeps them from being
/// re-examined as items in their own right.
fn buffer_items(ops: &[RawOp]) -> (Vec<ParsedItem>, usize) {
    let mut items = Vec::new();
    let mut skipped = 0usize;
    let mut i = 0usize;

    while i < ops.len() {
        if !looks_like_format_op(&ops[i].data) {
            i += 1;
            continue;
        }
        let format = ops[i].data.clone();
        let data_op_count = le16(&format, at::SIZE) as usize - 1;
        if i + data_op_count >= ops.len() {
            break;
        }

        let mut data_ops = Vec::with_capacity(data_op_count);
        let mut split = false;
        for op in &ops[i + 1..=i + data_op_count] {
            if op.flags & (OP_CONTINUES | OP_WAS_CONTINUED) != 0 {
                split = true;
            }
            data_ops.push(op.data.clone());
        }

        if split {
            skipped += 1;
        } else {
            items.push(ParsedItem { format, data_ops });
        }
        i += 1 + data_op_count;
    }
    (items, skipped)
}

/// The runs of set bits in a format operation's bitmap, as
/// `(first chunk, chunk count)`.
fn runs_of(format: &[u8]) -> Vec<(usize, usize)> {
    let map_size = le32(format, at::MAP_SIZE) as usize;
    let chunks = map_size * 32;
    let bit = |c: usize| {
        let word = le32(format, at::DATA_MAP + (c / 32) * 4);
        word & (1 << (c % 32)) != 0
    };

    let mut out = Vec::new();
    let mut c = 0;
    while c < chunks {
        if !bit(c) {
            c += 1;
            continue;
        }
        let start = c;
        while c < chunks && bit(c) {
            c += 1;
        }
        out.push((start, c - start));
    }
    out
}

/// Rebuild an item through the encoder and hand back what it produced.
///
/// The reconstruction starts from a zeroed buffer and writes only the
/// logged chunks into it, because the logged chunks are all a reader has
/// — which is also the point. If the encoder needed anything more than
/// the address, the dirty chunks and their contents to reproduce the
/// item, it would not be able to write one.
fn reencode(item: &ParsedItem) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let format = &item.format;
    let blkno = u64::from_le_bytes(format[at::BLKNO..at::BLKNO + 8].try_into().unwrap());
    let len_blocks = le16(format, at::LEN) as usize;
    let raw_flags = le16(format, at::FLAGS);
    let buf_type = raw_flags >> BLF_TYPE_SHIFT;
    let flags = raw_flags & BLF_FLAG_MASK;

    if len_blocks == 0 {
        return None;
    }

    let runs = runs_of(format);
    // A cancel says everything with its address and its flag, and the
    // encoder has a constructor for exactly that shape.
    if runs.is_empty() {
        let rebuilt = BufferItem::cancel(blkno, len_blocks as u32);
        let ops = rebuilt.ops();
        return Some((ops[0].data.clone(), Vec::new()));
    }

    let mut rebuilt = BufferItem::new(blkno, vec![0u8; len_blocks * BBSIZE], buf_type, flags);
    for (run, &(start, count)) in runs.iter().enumerate() {
        let bytes = item.data_ops.get(run)?;
        if bytes.len() != count * BLF_CHUNK {
            return None;
        }
        rebuilt.edit(start * BLF_CHUNK, bytes);
    }

    let ops = rebuilt.ops();
    Some((
        ops[0].data.clone(),
        ops[1..].iter().map(|op| op.data.clone()).collect(),
    ))
}

/// Every fixture whose log might hold buffer items, most-populated
/// first. The stress images are where the bulk of them are: ordinary
/// mkfs images have a log the kernel has barely touched.
const IMAGES: &[&str] = &[
    "xfsstress-fsx.img",
    "xfsstress-ops.img",
    "xfsstress-ops1k.img",
    "xfsdirty.img",
    "xfsx-rename-after.img",
    "xfsdata-default.img",
    "xfsdata-1k.img",
    "xfsdata-ftype.img",
];

/// The encoder must reproduce, byte for byte, every buffer item the
/// kernel wrote.
#[test]
fn every_kernel_buffer_item_re_encodes_identically() {
    let mut total = 0usize;
    let mut split = 0usize;
    let mut images_read = 0usize;

    for name in IMAGES {
        let path = share().join(name);
        if !path.exists() {
            continue;
        }
        let Some(ops) = ops_in_log(&path) else {
            continue;
        };
        images_read += 1;

        let (items, skipped) = buffer_items(&ops);
        split += skipped;

        for (n, item) in items.iter().enumerate() {
            let Some((format, data_ops)) = reencode(item) else {
                panic!("{name} item {n}: could not be rebuilt from its own logged contents");
            };

            assert_eq!(
                format, item.format,
                "{name} item {n}: the format operation differs.\n \
                 kernel: {:02x?}\n encoder: {:02x?}",
                item.format, format
            );
            assert_eq!(
                data_ops.len(),
                item.data_ops.len(),
                "{name} item {n}: {} data operations against the kernel's {}",
                data_ops.len(),
                item.data_ops.len()
            );
            for (k, (ours, theirs)) in data_ops.iter().zip(&item.data_ops).enumerate() {
                assert_eq!(
                    ours.len(),
                    theirs.len(),
                    "{name} item {n} operation {k}: length differs"
                );
                assert_eq!(ours, theirs, "{name} item {n} operation {k}: bytes differ");
            }
            total += 1;
        }
    }

    if images_read == 0 {
        eprintln!(
            "no fixture images found in {}; build them with \
             ./scripts/vm-build-log-fixtures.sh",
            share().display()
        );
        return;
    }

    // A test that silently matched nothing would pass just as loudly as
    // one that matched everything.
    assert!(
        total > 100,
        "only {total} buffer items were found across {images_read} images, \
         which is too few to have exercised the encoder"
    );
    eprintln!("re-encoded {total} kernel buffer items ({split} skipped as split across records)");
}

/// The invariants the format document states, checked against the
/// kernel's own items rather than against the encoder's.
///
/// These are what a parser is entitled to rely on, so a fixture that
/// broke one would mean the document is wrong — not the encoder.
#[test]
fn the_kernels_items_satisfy_the_documented_invariants() {
    let mut checked = 0usize;

    for name in IMAGES {
        let path = share().join(name);
        if !path.exists() {
            continue;
        }
        let Some(ops) = ops_in_log(&path) else {
            continue;
        };
        let (items, _) = buffer_items(&ops);

        for item in &items {
            let map_size = le32(&item.format, at::MAP_SIZE) as usize;
            assert_eq!(
                item.format.len(),
                BLF_HEADER_SIZE + 4 * map_size,
                "{name}: op_len == 20 + 4 * map_size"
            );

            let set: u32 = item.format[at::DATA_MAP..]
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()).count_ones())
                .sum();
            let logged: usize = item.data_ops.iter().map(|d| d.len()).sum();
            assert_eq!(
                set as usize * BLF_CHUNK,
                logged,
                "{name}: popcount(map) * 128 == the data operations' total length"
            );

            // One data operation per run, which is what makes the item
            // parseable at all.
            assert_eq!(
                runs_of(&item.format).len(),
                item.data_ops.len(),
                "{name}: one data operation per run of set bits"
            );
            checked += 1;
        }
    }

    if checked > 0 {
        eprintln!("{checked} kernel buffer items satisfy the documented invariants");
    }
}
