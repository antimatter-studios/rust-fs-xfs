//! Writing the buffer log item.
//!
//! An inode change can be logged by handing the log a copy of the
//! inode. Everything else XFS does cannot: an allocation moves a few
//! bytes in an allocation-group header and a few more in each of two
//! B+trees, and copying those blocks whole would put kilobytes in the
//! log for a change of a few dozen bytes.
//!
//! So XFS logs *which bytes of which buffer changed*. The item names a
//! block by its absolute device address, carries a bitmap with one bit
//! per 128-byte chunk of it, and follows that with the contents of just
//! the chunks whose bits are set. Allocation, truncate, create, unlink
//! and any directory past shortform all rest on this one structure.
//!
//! # What an item looks like on the wire
//!
//! ```text
//! op  len   what
//!  0    20+ xfs_buf_log_format — which block, which chunks
//!  1   128× the first run of dirty chunks
//!  2   128× the second run
//! ```
//!
//! One data operation per **maximal run of consecutive set bits**, in
//! ascending order, each exactly `run length × 128` bytes. The format
//! operation's `blf_size` counts itself plus those runs, which is what
//! lets a reader know how many operations to consume before the next
//! item begins.
//!
//! # Three things that are easy to get wrong
//!
//! **The address is in 512-byte basic blocks**, absolute on the device.
//! Not filesystem blocks and not sectors — both were ruled out by
//! varying them independently, and the AGI stayed at basic block 2 while
//! the filesystem block size changed under it. See
//! [`crate::format::log_items::buf_log_format::BLF_BLKNO_UNIT`].
//!
//! **The structure is little-endian**, inside a record whose framing is
//! big-endian and on a filesystem that is big-endian nearly everywhere
//! else.
//!
//! **There is no padding after the bitmap.** The next operation header
//! begins immediately at `20 + 4 × map_size`, and operation payloads are
//! not aligned at all — a 2156-byte operation was observed. Rounding the
//! format operation up to a multiple of four produces a record that
//! checksums and whose every subsequent item is misread.
//!
//! # Why the chunk granularity is not a detail
//!
//! Marking one byte dirty logs the whole 128-byte chunk around it, so
//! the chunk contents have to be *correct*, not merely contain the
//! intended change. A caller that patches four bytes into a stale copy
//! of a block and marks it dirty writes the stale 124 bytes around them
//! into the log as though they were current — which is exactly the trap
//! the kernel's own unlink lands in, and the reason a replayer must not
//! apply an inode chunk verbatim.
//!
//! [`BufferItem::edit`] takes the block's current contents for this
//! reason: the dirty region is defined by what was written into it,
//! rather than by the caller separately remembering to mark it.

use crate::format::log_items::buf_log_format::{
    flags::BLF_FLAG_MASK, offsets as at, BLF_CHUNK, BLF_HEADER_SIZE, BLF_TYPE_SHIFT,
};
use crate::format::log_items::item_types::XFS_LI_BUF;
use crate::log::BBSIZE;
use crate::log_write::Op;

/// Bits of bitmap carried by one 32-bit word.
const CHUNKS_PER_WORD: usize = 32;

/// One buffer, the bytes it now holds, and which chunks of it changed.
///
/// Built around the block's **current contents** rather than around a
/// list of edits, because the log records chunks and not edits: the
/// question the encoder has to answer is what each dirty chunk should
/// now contain, and only a full copy of the block can answer it.
#[derive(Debug, Clone)]
pub struct BufferItem {
    /// Absolute device address in 512-byte basic blocks.
    blkno: u64,
    /// The buffer-type code, which goes in the top five bits of
    /// `blf_flags`.
    buf_type: u16,
    /// The low flag bits, below the buffer type.
    flags: u16,
    /// The whole buffer, of which the dirty chunks will be logged.
    data: Vec<u8>,
    /// One bit per [`BLF_CHUNK`]-byte chunk, LSB-first within each word.
    dirty: Vec<u32>,
}

impl BufferItem {
    /// Start an item for the block at `blkno` holding `data`.
    ///
    /// Nothing is dirty yet. `data` is the block as it currently reads;
    /// call [`BufferItem::edit`] to change part of it, which is what
    /// marks that part for logging.
    ///
    /// # Panics
    ///
    /// If `data` is not a whole number of basic blocks, since `blf_len`
    /// has no way to express anything else.
    pub fn new(blkno: u64, data: Vec<u8>, buf_type: u16, flags: u16) -> Self {
        assert!(
            data.len().is_multiple_of(BBSIZE) && !data.is_empty(),
            "a buffer is a whole number of {BBSIZE}-byte basic blocks, not {} bytes",
            data.len()
        );
        let words = chunk_count(data.len()).div_ceil(CHUNKS_PER_WORD);
        BufferItem {
            blkno,
            buf_type,
            flags: flags & BLF_FLAG_MASK,
            data,
            dirty: vec![0; words],
        }
    }

    /// The cancel item: this block is being freed or reused, and any
    /// earlier item logging it must not be replayed.
    ///
    /// It carries no data and no dirty chunks — the address and the flag
    /// are the whole message. `len_blocks` is in basic blocks.
    pub fn cancel(blkno: u64, len_blocks: u32) -> Self {
        use crate::format::log_items::buf_log_format::{buf_type::BLFT_NONE, flags::BLF_CANCEL};

        let mut item = BufferItem::new(
            blkno,
            vec![0u8; len_blocks as usize * BBSIZE],
            BLFT_NONE,
            BLF_CANCEL,
        );
        // A cancel is the one item whose bitmap is meaningful by being
        // empty, so the data it was given is never logged.
        item.dirty.iter_mut().for_each(|w| *w = 0);
        item
    }

    /// Overwrite `offset..offset + bytes.len()` and mark every chunk it
    /// touches dirty.
    ///
    /// The whole of each touched chunk is logged, including the bytes
    /// around the change — which is why this writes into the block
    /// rather than recording the change separately. The surrounding
    /// bytes go into the log either way; this way they are the ones the
    /// block actually holds.
    ///
    /// # Panics
    ///
    /// If the range runs past the end of the buffer.
    pub fn edit(&mut self, offset: usize, bytes: &[u8]) {
        assert!(
            offset + bytes.len() <= self.data.len(),
            "the edit ends at {} and the buffer is {} bytes",
            offset + bytes.len(),
            self.data.len()
        );
        if bytes.is_empty() {
            return;
        }
        self.data[offset..offset + bytes.len()].copy_from_slice(bytes);
        self.mark(offset, bytes.len());
    }

    /// Mark `offset..offset + len` dirty without changing it.
    ///
    /// For the case where the bytes are already what they should be —
    /// re-logging a block built elsewhere — and for reconstructing an
    /// item that was read out of a log.
    ///
    /// # Panics
    ///
    /// If the range runs past the end of the buffer.
    pub fn mark(&mut self, offset: usize, len: usize) {
        assert!(
            offset + len <= self.data.len(),
            "the range ends at {} and the buffer is {} bytes",
            offset + len,
            self.data.len()
        );
        if len == 0 {
            return;
        }
        let first = offset / BLF_CHUNK;
        let last = (offset + len - 1) / BLF_CHUNK;
        for chunk in first..=last {
            self.dirty[chunk / CHUNKS_PER_WORD] |= 1 << (chunk % CHUNKS_PER_WORD);
        }
    }

    /// The buffer's contents, as they will be logged.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Whether chunk `c` is marked dirty.
    fn is_dirty(&self, chunk: usize) -> bool {
        self.dirty[chunk / CHUNKS_PER_WORD] & (1 << (chunk % CHUNKS_PER_WORD)) != 0
    }

    /// The maximal runs of consecutive dirty chunks, as
    /// `(first chunk, chunk count)`, in ascending order.
    ///
    /// One data operation per run is the rule the whole item rests on,
    /// so this is the only place that decides how many operations the
    /// item occupies.
    fn runs(&self) -> Vec<(usize, usize)> {
        let chunks = chunk_count(self.data.len());
        let mut out = Vec::new();
        let mut chunk = 0;
        while chunk < chunks {
            if !self.is_dirty(chunk) {
                chunk += 1;
                continue;
            }
            let start = chunk;
            while chunk < chunks && self.is_dirty(chunk) {
                chunk += 1;
            }
            out.push((start, chunk - start));
        }
        out
    }

    /// The operations this item contributes to a transaction: the format
    /// operation, then one per run of dirty chunks.
    ///
    /// The count is also what the transaction header has to include in
    /// its own tally — that field counts *operations belonging to items*,
    /// so an item with two runs contributes three.
    pub fn ops(&self) -> Vec<Op> {
        let runs = self.runs();
        let mut ops = Vec::with_capacity(1 + runs.len());
        ops.push(Op {
            flags: 0,
            data: self.format_op(&runs),
        });
        for &(start, len) in &runs {
            let from = start * BLF_CHUNK;
            ops.push(Op {
                flags: 0,
                data: self.data[from..from + len * BLF_CHUNK].to_vec(),
            });
        }
        ops
    }

    /// How many operations [`BufferItem::ops`] will produce.
    pub fn op_count(&self) -> usize {
        1 + self.runs().len()
    }

    /// `xfs_buf_log_format`: which block, how much of it, and the bitmap.
    fn format_op(&self, runs: &[(usize, usize)]) -> Vec<u8> {
        let map_words = self.dirty.len();
        let mut v = vec![0u8; BLF_HEADER_SIZE + 4 * map_words];

        v[at::TYPE..at::TYPE + 2].copy_from_slice(&XFS_LI_BUF.to_le_bytes());
        // This operation plus one per run: what a reader consumes before
        // the next item starts.
        let size = u16::try_from(1 + runs.len()).expect("an item cannot span 65536 operations");
        v[at::SIZE..at::SIZE + 2].copy_from_slice(&size.to_le_bytes());
        let flags = self.flags | (self.buf_type << BLF_TYPE_SHIFT);
        v[at::FLAGS..at::FLAGS + 2].copy_from_slice(&flags.to_le_bytes());
        let len = u16::try_from(self.data.len() / BBSIZE).expect("a buffer under 32 MiB");
        v[at::LEN..at::LEN + 2].copy_from_slice(&len.to_le_bytes());
        v[at::BLKNO..at::BLKNO + 8].copy_from_slice(&self.blkno.to_le_bytes());
        v[at::MAP_SIZE..at::MAP_SIZE + 4].copy_from_slice(&(map_words as u32).to_le_bytes());
        for (i, word) in self.dirty.iter().enumerate() {
            let off = at::DATA_MAP + i * 4;
            v[off..off + 4].copy_from_slice(&word.to_le_bytes());
        }
        v
    }
}

/// How many [`BLF_CHUNK`]-byte chunks a buffer of `bytes` holds.
///
/// Rounded up, so a buffer that is not a whole number of chunks still
/// has a bit covering its tail. A 512-byte basic block is four chunks
/// exactly, so this only rounds for geometries that do not exist yet.
fn chunk_count(bytes: usize) -> usize {
    bytes.div_ceil(BLF_CHUNK)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::log_items::buf_log_format::buf_type::BLFT_AGF;

    fn block(n: usize) -> Vec<u8> {
        (0..n * BBSIZE).map(|i| (i % 251) as u8).collect()
    }

    /// An edit that fits inside one chunk logs that chunk and no other,
    /// and the operation is exactly one chunk long.
    #[test]
    fn one_edit_logs_one_chunk() {
        let mut item = BufferItem::new(8, block(8), BLFT_AGF, 0);
        item.edit(BLF_CHUNK + 4, &[0xaa; 4]);

        let ops = item.ops();
        assert_eq!(ops.len(), 2, "format operation plus one run");
        assert_eq!(ops[1].data.len(), BLF_CHUNK);
        assert_eq!(ops[1].data[4..8], [0xaa; 4]);
    }

    /// Two edits far apart are two runs and two operations; adjacent
    /// ones are a single run and a single operation. The distinction is
    /// the whole of the encoding's compactness.
    #[test]
    fn runs_are_maximal() {
        let mut apart = BufferItem::new(8, block(8), BLFT_AGF, 0);
        apart.edit(0, &[1; 4]);
        apart.edit(BLF_CHUNK * 10, &[1; 4]);
        assert_eq!(apart.op_count(), 3, "two separated chunks are two runs");

        let mut adjacent = BufferItem::new(8, block(8), BLFT_AGF, 0);
        adjacent.edit(0, &[1; 4]);
        adjacent.edit(BLF_CHUNK, &[1; 4]);
        let ops = adjacent.ops();
        assert_eq!(ops.len(), 2, "two touching chunks are one run");
        assert_eq!(ops[1].data.len(), BLF_CHUNK * 2);
    }

    /// An edit spanning a chunk boundary dirties both chunks, including
    /// the bytes either side of it that were never written.
    #[test]
    fn an_edit_across_a_boundary_logs_both_chunks() {
        let mut item = BufferItem::new(8, block(8), BLFT_AGF, 0);
        item.edit(BLF_CHUNK - 2, &[0xff; 4]);

        let ops = item.ops();
        assert_eq!(ops[1].data.len(), BLF_CHUNK * 2);
        // The run starts at the chunk before the edit, so the logged
        // bytes begin with what the block already held.
        assert_eq!(ops[1].data[0], block(8)[0]);
    }

    /// The two invariants the format guarantees, which a parser is
    /// entitled to assert.
    #[test]
    fn the_documented_invariants_hold() {
        let mut item = BufferItem::new(8, block(16), BLFT_AGF, 0);
        item.edit(0, &[1; 8]);
        item.edit(BLF_CHUNK * 5, &[1; 300]);
        item.edit(BLF_CHUNK * 60, &[1; 8]);

        let ops = item.ops();
        let map_size = u32::from_le_bytes(
            ops[0].data[at::MAP_SIZE..at::MAP_SIZE + 4]
                .try_into()
                .unwrap(),
        ) as usize;

        assert_eq!(
            ops[0].data.len(),
            BLF_HEADER_SIZE + 4 * map_size,
            "op_len == 20 + 4 * map_size"
        );

        let set: u32 = ops[0].data[at::DATA_MAP..]
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()).count_ones())
            .sum();
        let logged: usize = ops[1..].iter().map(|op| op.data.len()).sum();
        assert_eq!(
            set as usize * BLF_CHUNK,
            logged,
            "popcount(map) * 128 == sum of the data-operation lengths"
        );
    }

    /// A cancel names a block and says nothing else: no chunks, no data
    /// operations, one operation in total.
    #[test]
    fn a_cancel_carries_no_data() {
        use crate::format::log_items::buf_log_format::flags::BLF_CANCEL;

        let item = BufferItem::cancel(64, 8);
        let ops = item.ops();
        assert_eq!(ops.len(), 1, "a cancel is the format operation alone");

        let size = u16::from_le_bytes(ops[0].data[at::SIZE..at::SIZE + 2].try_into().unwrap());
        assert_eq!(size, 1);
        let flags = u16::from_le_bytes(ops[0].data[at::FLAGS..at::FLAGS + 2].try_into().unwrap());
        assert_eq!(flags & BLF_FLAG_MASK, BLF_CANCEL);
    }

    /// The address is written as given — in basic blocks, absolute — and
    /// the length is the buffer in basic blocks rather than bytes.
    #[test]
    fn the_address_is_basic_blocks() {
        let item = BufferItem::new(4096, block(8), BLFT_AGF, 0);
        let ops = item.ops();
        let blkno = u64::from_le_bytes(ops[0].data[at::BLKNO..at::BLKNO + 8].try_into().unwrap());
        let len = u16::from_le_bytes(ops[0].data[at::LEN..at::LEN + 2].try_into().unwrap());
        assert_eq!(blkno, 4096);
        assert_eq!(len, 8, "eight basic blocks, not 4096 bytes");
    }

    /// The buffer type lives in the top five bits and the flags below
    /// it, so neither can corrupt the other.
    #[test]
    fn the_type_and_the_flags_share_one_field() {
        use crate::format::log_items::buf_log_format::{buf_type::BLFT_DINO, flags::BLF_INODE_BUF};

        let item = BufferItem::new(8, block(8), BLFT_DINO, BLF_INODE_BUF);
        let ops = item.ops();
        let raw = u16::from_le_bytes(ops[0].data[at::FLAGS..at::FLAGS + 2].try_into().unwrap());
        assert_eq!(raw & BLF_FLAG_MASK, BLF_INODE_BUF);
        assert_eq!(raw >> BLF_TYPE_SHIFT, BLFT_DINO);
    }
}
