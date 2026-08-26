//! Symbolic links, and the block header a long one is stored behind.
//!
//! A short target lives in the inode's data fork and is nothing but the
//! bytes. A long one is stored in filesystem blocks, and on v5 those
//! blocks begin with a self-describing header — so the target is **not**
//! simply the file's contents, and reading it as though it were yields
//! the header's magic followed by a truncated target.
//!
//! That failure is quiet. `di_size` is the target's length, not the
//! blocks' length, so a reader that takes `di_size` bytes from the start
//! of the data gets something of exactly the right length that begins
//! `XSLM` and ends early. It looks like a path, and it is not one.
//!
//! # One header per extent, not per block
//!
//! This is the part that is easy to get wrong, and it fails in a way
//! that looks like corruption rather than a misunderstanding.
//!
//! The unit is the **extent**, not the block. A target's blocks are
//! written as one buffer per contiguous run, and only the first block of
//! each run carries a header. [`offsets::BYTES`] is how much of the
//! target that whole run holds — not how much its first block holds —
//! and the CRC covers the run's entire byte length, every block of it.
//!
//! Measured. A 975-byte target on a 1 KiB-block filesystem occupies one
//! two-block extent: `sl_bytes` reads 975, the second block carries no
//! magic at all, and the stored checksum reproduces over 2048 bytes and
//! no other span. Assuming a header per block instead finds a symlink
//! block where the target's own second half is, and assuming the
//! checksum covers one block fails it outright.
//!
//! At a 4 KiB block size none of this shows: the longest target a
//! generator produces still fits one block, and per-block and per-extent
//! are the same thing. That is why the stress corpus is built at 1 KiB
//! as well.
//!
//! # Provenance
//!
//! The published specification documents the v4 arrangement — target
//! bytes in the blocks, nothing else. The header is a v5 addition and is
//! not in it. The layout here was read off filesystems the kernel wrote:
//! 15 remote symlinks in the stress corpus at 4 KiB and the same tree
//! again at 1 KiB, whose targets are known independently because the
//! kernel listed them.

/// `XFS_SYMLINK_MAGIC` — `XSLM`, at the start of every v5 symlink block.
pub const XFS_SYMLINK_MAGIC: u32 = 0x5853_4c4d;

/// `sizeof(struct xfs_dsymlink_hdr)`.
pub const XFS_SYMLINK_HDR_SIZE: usize = 56;

/// `XFS_SYMLINK_MAXLEN` — the longest target XFS stores.
///
/// One byte short of the 1025 a `PATH_MAX` target would need, and the
/// reason a symlink never needs more than a couple of blocks.
pub const XFS_SYMLINK_MAXLEN: usize = 1024;

/// Byte offsets within `xfs_dsymlink_hdr`. Big-endian, like every other
/// on-disk structure.
pub mod offsets {
    /// `sl_magic`, `u32` — [`super::XFS_SYMLINK_MAGIC`].
    pub const MAGIC: usize = 0;
    /// `sl_offset`, `u32` — where this block's bytes begin within the
    /// target. Blocks are written in order, so this is the running total
    /// of the `sl_bytes` before it.
    pub const OFFSET: usize = 4;
    /// `sl_bytes`, `u32` — how much of the target this block holds. The
    /// last block is short; the rest are full.
    pub const BYTES: usize = 8;
    /// `sl_crc`, `u32` — CRC32C over the whole block with this field
    /// zeroed, stored little-endian like every other XFS checksum.
    pub const CRC: usize = 12;
    /// `sl_uuid`, 16 bytes — the filesystem's own.
    pub const UUID: usize = 16;
    /// `sl_owner`, `u64` — the inode this block belongs to. What
    /// distinguishes an intact block that came from the wrong place from
    /// one that is merely corrupt.
    pub const OWNER: usize = 32;
    /// `sl_blkno`, `u64` — the block's own address, in basic blocks.
    pub const BLKNO: usize = 40;
    /// `sl_lsn`, `u64` — the log sequence number that last wrote it.
    pub const LSN: usize = 48;
}

/// How much of a target a buffer of `bytes` holds.
///
/// `bytes` is a whole extent's worth, since that is the unit a header
/// covers. v4 buffers are raw, so the answer is all of it. A v5 buffer
/// gives up its first [`XFS_SYMLINK_HDR_SIZE`] bytes to the header —
/// once, at the front, however many blocks follow.
pub const fn buf_space(bytes: usize, v5: bool) -> usize {
    if v5 {
        bytes - XFS_SYMLINK_HDR_SIZE
    } else {
        bytes
    }
}
