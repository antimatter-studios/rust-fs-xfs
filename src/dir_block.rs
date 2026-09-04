//! Building a block-form directory.
//!
//! A short-form directory lives inside its inode. When one more entry
//! will not fit, XFS allocates a filesystem block and writes the whole
//! directory into it: `.`, `..`, every existing name and the new one,
//! followed by a hash index, all in the same block. This builds that
//! block.
//!
//! It is the conversion every write in this driver currently refuses,
//! and it is the reason a directory of about 30 short names is the
//! ceiling on everything else.
//!
//! # The shape
//!
//! ```text
//!    0  header          64 bytes on v5: magic, checksum, own address,
//!                       sequence number, filesystem UUID, owning inode
//!   48  bestfree[3]     the three largest free regions, largest first
//!   64  entries         `.`, `..`, then the names, growing forwards
//!  ...  one free record filling the middle
//!  ...  hash index      8 bytes per entry, growing backwards
//!  -8   tail            how many index records, how many are stale
//! ```
//!
//! Entries grow forwards from the header and the index grows backwards
//! from the tail, with a single free region between them. That is why a
//! block-form directory has exactly one free record when it is built:
//! everything is packed, and the gap is whatever is left.
//!
//! # Four things that are easy to get wrong
//!
//! **`.` and `..` are real entries here.** Short form keeps the parent
//! in its header and neither as an entry; block form materialises both,
//! and they come first. A conversion that carries across only the
//! short-form entries produces a directory with no `..`, which reads
//! correctly right up until something walks upwards through it.
//!
//! **Every entry ends with a tag repeating its own offset.** It is
//! redundant by construction, which is exactly what makes it the
//! cheapest detector of a walk that has lost alignment. Writing it
//! wrong produces a block this driver's own reader rejects.
//!
//! **The index is sorted by hash, never by name.** Equal hashes are
//! permitted, so a lookup binary-searches to a *range* and compares
//! names within it. Sorting by name instead would produce an index that
//! looks ordered and finds nothing.
//!
//! **An address is a byte offset divided by eight**, not a byte offset.
//! `.` is always the first entry of the block, so its address is the
//! header size over eight — 8 on v5. That constant is a useful check
//! that the units have not been confused.
//!
//! # The checksum is deliberately not computed
//!
//! As everywhere else in this driver's write path, a logged block
//! carries a stale checksum and recovery recomputes it on write-out.
//! See [`crate::group_write::restamp_crc`].

use crate::dir;
use crate::error::{Error, Result};
use crate::format::attr::hashname;
use crate::format::dir::{
    offsets, XFS_DIR2_BLOCK_TAIL_SIZE, XFS_DIR2_DATA_ALIGN, XFS_DIR2_DATA_FREE_TAG,
    XFS_DIR2_LEAF_ENTRY_SIZE, XFS_DIR3_BLOCK_MAGIC, XFS_DIR3_DATA_HDR_SIZE,
};
use crate::superblock::Superblock;

/// One entry to place in the block.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: Vec<u8>,
    pub ino: u64,
    /// The on-disk file-type byte, as [`crate::dir::ftype_to_raw`]
    /// returns it.
    pub ftype: u8,
}

/// How many bytes an entry occupies, rounded as the format requires.
///
/// Inode number, name length, the name, the file-type byte and the tag,
/// rounded up to eight. The type byte is unconditional here because a v5
/// filesystem always has the feature — `mkfs.xfs` refuses `-n ftype=0`
/// alongside `-m crc=1` — and v5 is the only version this writes.
pub fn entry_size(namelen: usize) -> usize {
    let unrounded = 8 + 1 + namelen + 1 + 2;
    unrounded.div_ceil(XFS_DIR2_DATA_ALIGN) * XFS_DIR2_DATA_ALIGN
}

/// The smallest block that could hold these entries plus their index.
///
/// Used to refuse a conversion that would not fit rather than write a
/// block whose index has overrun its entries.
pub fn space_needed(entries: &[Entry]) -> usize {
    let data: usize = entries.iter().map(|e| entry_size(e.name.len())).sum();
    XFS_DIR3_DATA_HDR_SIZE
        + data
        + entries.len() * XFS_DIR2_LEAF_ENTRY_SIZE
        + XFS_DIR2_BLOCK_TAIL_SIZE
}

/// Build the block-form directory block for `entries`.
///
/// `entries` must already include `.` and `..`, in that order and first
/// — this does not invent them, because which inode `..` names is the
/// caller's business and getting it wrong is not something this could
/// detect.
///
/// `fsblock` is where the block will live, used for the address it
/// records about itself, and `owner` is the directory's inode number.
///
/// # Errors
///
/// [`Error::UnsupportedFeature`] if the entries and their index do not
/// fit in one directory block — that is the leaf form, which needs more
/// than one block and an index block of its own.
pub fn build(sb: &Superblock, fsblock: u64, owner: u64, entries: &[Entry]) -> Result<Vec<u8>> {
    let dirblocksize = (u64::from(sb.blocksize) << sb.dirblklog) as usize;

    let needed = space_needed(entries);
    if needed > dirblocksize {
        return Err(Error::UnsupportedFeature(format!(
            "inode {owner}'s {} entries need {needed} bytes and a directory block holds \
             {dirblocksize}; a directory past one block is the leaf form, which is not \
             implemented",
            entries.len()
        )));
    }

    let mut block = vec![0u8; dirblocksize];

    // ---- header ---------------------------------------------------------
    use offsets::dir3_blk as h;
    block[h::MAGIC..h::MAGIC + 4].copy_from_slice(&XFS_DIR3_BLOCK_MAGIC.to_be_bytes());
    // The block records its own address in 512-byte basic blocks, which
    // is what catches a block that was written to the wrong place.
    //
    // `fsblock` is PACKED -- agno above sb_agblklog bits of agbno -- so
    // it has to be unpacked before it means an address. Multiplying it
    // straight through is right only while agblocks is a power of two,
    // and mkfs.xfs sizes groups to the device.
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[h::BLKNO..h::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[h::UUID..h::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[h::OWNER..h::OWNER + 8].copy_from_slice(&owner.to_be_bytes());
    // `lsn` and `crc` are left zero. Recovery stamps the first and
    // recomputes the second.

    // ---- entries --------------------------------------------------------
    //
    // Placed in the order given. The kernel keeps `.`, `..` and then the
    // short-form entries in their existing order, appending the new one,
    // and the index below is what makes any order findable — but the
    // order still has to be reproduced to match the kernel byte for
    // byte.
    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    let mut index: Vec<(u32, u32)> = Vec::with_capacity(entries.len());

    for entry in entries {
        let namelen = entry.name.len();
        let len = entry_size(namelen);
        use offsets::data_entry as d;

        block[at + d::INUMBER..at + d::INUMBER + 8].copy_from_slice(&entry.ino.to_be_bytes());
        block[at + d::NAMELEN] = u8::try_from(namelen).map_err(|_| {
            Error::UnsupportedFeature(format!("a name of {namelen} bytes is too long"))
        })?;
        block[at + d::NAME..at + d::NAME + namelen].copy_from_slice(&entry.name);
        block[at + d::NAME + namelen] = entry.ftype;
        // The tag repeats the entry's own offset, at the end of the
        // rounded record rather than at the end of the fields.
        let tag = u16::try_from(at).expect("a directory block is at most 64 KiB");
        block[at + len - 2..at + len].copy_from_slice(&tag.to_be_bytes());

        // An address is the byte offset over eight.
        index.push((
            hashname(&entry.name),
            u32::try_from(at / XFS_DIR2_DATA_ALIGN).expect("fits"),
        ));
        at += len;
    }
    let entries_end = at;

    // ---- the index, and the free region between it and the entries ------
    let tail_at = dirblocksize - XFS_DIR2_BLOCK_TAIL_SIZE;
    let index_start = tail_at - entries.len() * XFS_DIR2_LEAF_ENTRY_SIZE;

    // Sorted by hash. Equal hashes are permitted and their relative
    // order is not specified, so this is a stable sort: two names that
    // collide keep the order they were placed in, which is the order the
    // kernel placed them in too.
    index.sort_by_key(|&(hash, _)| hash);

    for (i, &(hash, addr)) in index.iter().enumerate() {
        let at = index_start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        use offsets::leaf_entry as l;
        block[at + l::HASHVAL..at + l::HASHVAL + 4].copy_from_slice(&hash.to_be_bytes());
        block[at + l::ADDRESS..at + l::ADDRESS + 4].copy_from_slice(&addr.to_be_bytes());
    }

    // Everything between the last entry and the first index record is
    // one free record. A freshly built block has exactly one, because
    // the entries are packed and the gap is whatever is left over.
    let free_len = index_start - entries_end;
    if free_len > 0 {
        use offsets::data_unused as u;
        block[entries_end + u::FREETAG..entries_end + u::FREETAG + 2]
            .copy_from_slice(&XFS_DIR2_DATA_FREE_TAG.to_be_bytes());
        let len = u16::try_from(free_len).expect("a directory block is at most 64 KiB");
        block[entries_end + u::LENGTH..entries_end + u::LENGTH + 2]
            .copy_from_slice(&len.to_be_bytes());
        let tag_at = entries_end + u::tag(free_len);
        let tag = u16::try_from(entries_end).expect("fits");
        block[tag_at..tag_at + 2].copy_from_slice(&tag.to_be_bytes());
    }

    // ---- best-free ------------------------------------------------------
    //
    // A cache of the three largest free regions, largest first, so that
    // adding an entry does not have to scan. There is only ever one when
    // the block is built, and the other two slots stay zero.
    let bf = offsets::data_hdr::V5_BESTFREE;
    if free_len > 0 {
        block[bf..bf + 2].copy_from_slice(&(entries_end as u16).to_be_bytes());
        block[bf + 2..bf + 4].copy_from_slice(&(free_len as u16).to_be_bytes());
    }
    // The other two best-free slots stay zero. There are three of them
    // (XFS_DIR2_DATA_FD_COUNT) at four bytes each
    // (XFS_DIR2_DATA_FREE_SIZE), and a freshly built block has only one
    // free region, so the remaining two are genuinely unused rather than
    // merely unwritten.

    // ---- tail -----------------------------------------------------------
    let count = u32::try_from(entries.len()).expect("fits");
    block[tail_at + offsets::block_tail::COUNT..tail_at + offsets::block_tail::COUNT + 4]
        .copy_from_slice(&count.to_be_bytes());
    // Nothing is stale in a block that has just been built.
    block[tail_at + offsets::block_tail::STALE..tail_at + offsets::block_tail::STALE + 4]
        .copy_from_slice(&0u32.to_be_bytes());

    Ok(block)
}

/// The entries a short-form directory becomes, with `.` and `..` in
/// front and `new` appended.
///
/// The order matters: it is the order the kernel writes, and the tests
/// compare against the kernel's own block byte for byte.
pub fn entries_from_short_form(
    parsed: &dir::ShortFormDir,
    self_ino: u64,
    new: Option<Entry>,
) -> Vec<Entry> {
    use crate::inode::FileType;

    let mut out = Vec::with_capacity(parsed.entries.len() + 3);
    // `.` and `..` are entries here and are not in short form at all.
    out.push(Entry {
        name: b".".to_vec(),
        ino: self_ino,
        ftype: dir::ftype_to_raw(Some(FileType::Directory)),
    });
    out.push(Entry {
        name: b"..".to_vec(),
        ino: parsed.parent_ino,
        ftype: dir::ftype_to_raw(Some(FileType::Directory)),
    });
    for e in &parsed.entries {
        out.push(Entry {
            name: e.name.clone(),
            ino: e.ino,
            ftype: dir::ftype_to_raw(e.ftype),
        });
    }
    out.extend(new);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rounding, including the case where the fields land exactly on
    /// a boundary and the case where they overshoot by one.
    #[test]
    fn an_entry_is_rounded_to_eight() {
        // 8 + 1 + namelen + 1 + 2 = 12 + namelen.
        assert_eq!(entry_size(1), 16, "13 rounds to 16");
        assert_eq!(entry_size(3), 16, "15 rounds to 16");
        assert_eq!(entry_size(4), 16, "16 is already a multiple");
        assert_eq!(entry_size(5), 24, "17 rounds to 24");
        assert_eq!(entry_size(12), 24, "24 is already a multiple");
        assert_eq!(entry_size(13), 32);
    }

    /// A 4 KiB v5 filesystem, enough for the geometry this module reads
    /// out of a superblock: the block size and the directory-block log.
    fn superblock() -> Superblock {
        superblock_with(1024)
    }

    /// A geometry whose groups are NOT a power of two blocks long, which
    /// is the ordinary case: `mkfs.xfs` sizes groups to the device and
    /// rounds `agblklog` up. A packed fsbno and a linear block number
    /// then differ from group 1 onward, and `superblock()` above cannot
    /// tell them apart because 1024 blocks with agblklog 10 makes them
    /// equal everywhere.
    fn ragged_superblock() -> Superblock {
        superblock_with(1000)
    }

    fn superblock_with(agblocks: u32) -> Superblock {
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
                                                         // Derived, so a geometry with ragged groups still adds up: the
                                                         // superblock parser refuses agcount * agblocks below dblocks.
        b[8..16].copy_from_slice(&(4u64 * u64::from(agblocks)).to_be_bytes()); // dblocks
        b[48..56].copy_from_slice(&4u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&agblocks.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
        b[96..100].copy_from_slice(&16u32.to_be_bytes()); // logblocks
        let versionnum = 5u16 | crate::superblock::version_flags::MOREBITSBIT;
        b[100..102].copy_from_slice(&versionnum.to_be_bytes());
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

    /// A directory that cannot fit in one block is refused rather than
    /// written, because the answer is the leaf form and not a truncated
    /// block.
    #[test]
    fn a_directory_too_big_for_one_block_is_refused() {
        let sb = superblock();
        // 255-byte names, which is the longest a name can be.
        let entries: Vec<Entry> = (0..40)
            .map(|i| Entry {
                name: vec![b'a' + (i % 26) as u8; 200],
                ino: 100 + i as u64,
                ftype: 1,
            })
            .collect();
        let err =
            build(&sb, 15, 131, &entries).expect_err("40 long names cannot fit in a 4 KiB block");
        assert!(
            err.to_string().contains("leaf form"),
            "the refusal should name what the answer would be: {err}"
        );
    }
    /// A directory block built for a group above the first records the
    /// address it will actually be written to.
    ///
    /// `bb_blkno` is what catches a block read from the wrong place, so
    /// a wrong one is worse than useless: on the write side the buffer
    /// log item carries the same number, and recovery writes the block
    /// where it says. The value has to come from unpacking the fsbno,
    /// not from multiplying it.
    ///
    /// This could not be caught before because `superblock()` gives
    /// every group 1024 blocks with `agblklog` 10, so packed and linear
    /// agree on it, and because the only fixtures that reached this code
    /// allocated in group 0.
    #[test]
    fn a_block_built_for_a_later_group_records_its_real_address() {
        let sb = ragged_superblock();
        let fsbno = (2u64 << sb.agblklog) | 7;
        let linear = 2 * u64::from(sb.agblocks) + 7;
        assert_ne!(fsbno, linear, "the geometry must make the two differ");

        let entries = [Entry {
            name: b"a".to_vec(),
            ino: 200,
            ftype: 1, // regular file, as ftype_to_raw encodes it
        }];
        let block = build(&sb, fsbno, 128, &entries).expect("build");

        use offsets::dir3_blk as h;
        let stated = u64::from_be_bytes(block[h::BLKNO..h::BLKNO + 8].try_into().unwrap());
        assert_eq!(
            stated,
            crate::alloc_btree::blkno_of_fsbno(&sb, fsbno),
            "the block should record where it will be written"
        );
        assert_ne!(
            stated,
            fsbno * u64::from(sb.blocksize) / crate::log::BBSIZE as u64,
            "and not what multiplying the packed number gives"
        );
    }
}
