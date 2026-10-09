//! Building a leaf-form directory (#366).
//!
//! A directory whose entries and hash index no longer fit in one block
//! leaves block form. Its entries go into **data blocks** at the start of
//! the directory's data space, and its hash index moves into a block of
//! its own, the **leaf**, at `XFS_DIR2_LEAF_OFFSET` (32 GiB into the
//! directory's file), which also carries `bests`: the longest free region
//! of each data block, so an insert can find room without reading every
//! block.
//!
//! ```text
//! data block   0  header      64 bytes: magic XDD3, checksum, own address,
//!                             sequence number, UUID, owner
//!             48  bestfree[3] the three largest free regions
//!             64  entries     `.` and `..` first in block 0, then names
//!            ...  one free region to the end of the block
//!
//! leaf block   0  blkinfo     56 bytes: siblings, magic 0x3df1, checksum,
//!                             own address, sequence number, UUID, owner
//!             56  count, stale
//!             64  hash index  8 bytes each, sorted by hash
//!            ...  unused
//!            ...  bests[]     2 bytes per data block
//!             -4  bestcount
//! ```
//!
//! Like [`crate::dir_block`], this lays a directory out whole from its
//! entries: every name packed in order into as few data blocks as hold
//! them, and an index with nothing stale. A directory rebuilt this way is
//! one the kernel reads, looks names up in and adds to, because each
//! block says exactly what is in it.
//!
//! # An address is a byte offset in the data space, divided by eight
//!
//! In block form it is the offset within the one block; here it is
//! `data block number × directory block size + offset within it`, over
//! eight. Getting the block number out of it is what makes a lookup land
//! in the right data block.
//!
//! # The checksums are not computed
//!
//! As for every logged block in this driver, recovery recomputes them.

use crate::dir_block::{entry_size, hash_for, Entry};
use crate::error::{Error, Result};
use crate::format::dir::{
    offsets, XFS_DIR2_BEST_SIZE, XFS_DIR2_DATA_ALIGN, XFS_DIR2_DATA_FREE_TAG,
    XFS_DIR2_LEAF_ENTRY_SIZE, XFS_DIR2_LEAF_TAIL_SIZE, XFS_DIR3_DATA_HDR_SIZE, XFS_DIR3_DATA_MAGIC,
    XFS_DIR3_LEAF1_MAGIC, XFS_DIR3_LEAF_HDR_SIZE,
};
use crate::superblock::Superblock;

/// Entries split into data blocks, in order, as many to a block as fit.
///
/// `entries` must begin with `.` and `..`, which belong at the front of
/// data block 0.
pub fn pack(entries: &[Entry], dirblocksize: usize) -> Result<Vec<Vec<Entry>>> {
    let room = dirblocksize - XFS_DIR3_DATA_HDR_SIZE;
    let mut blocks: Vec<Vec<Entry>> = vec![Vec::new()];
    let mut used = 0;
    for e in entries {
        let size = entry_size(e.name.len());
        if size > room {
            return Err(Error::UnsupportedFeature(format!(
                "an entry of {size} bytes does not fit in a {dirblocksize}-byte directory block"
            )));
        }
        if used + size > room {
            blocks.push(Vec::new());
            used = 0;
        }
        blocks.last_mut().expect("never empty").push(e.clone());
        used += size;
    }
    Ok(blocks)
}

/// How many index records, at most, a leaf block with `data_blocks`
/// entries in `bests` holds.
pub fn leaf_capacity(dirblocksize: usize, data_blocks: usize) -> usize {
    (dirblocksize
        - XFS_DIR3_LEAF_HDR_SIZE
        - XFS_DIR2_LEAF_TAIL_SIZE
        - data_blocks * XFS_DIR2_BEST_SIZE)
        / XFS_DIR2_LEAF_ENTRY_SIZE
}

/// One built data block: its bytes, its longest free region, and the
/// index records for its entries.
pub struct DataBlock {
    pub bytes: Vec<u8>,
    pub best: u16,
    pub index: Vec<(u32, u32)>,
}

/// Build data block `db` of the directory `owner`, holding `entries`, to
/// live at filesystem block `fsblock`.
pub fn build_data_block(
    sb: &Superblock,
    fsblock: u64,
    owner: u64,
    db: u64,
    entries: &[Entry],
) -> Result<DataBlock> {
    let dirblocksize = sb.dirblocksize() as usize;
    let mut block = vec![0u8; dirblocksize];
    use offsets::dir3_blk as h;
    block[h::MAGIC..h::MAGIC + 4].copy_from_slice(&XFS_DIR3_DATA_MAGIC.to_be_bytes());
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[h::BLKNO..h::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[h::UUID..h::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[h::OWNER..h::OWNER + 8].copy_from_slice(&owner.to_be_bytes());

    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    let mut index = Vec::with_capacity(entries.len());
    for e in entries {
        let namelen = e.name.len();
        let len = entry_size(namelen);
        if at + len > dirblocksize {
            return Err(Error::Internal(format!(
                "data block {db} of inode {owner} was given more entries than it holds"
            )));
        }
        use offsets::data_entry as d;
        block[at + d::INUMBER..at + d::INUMBER + 8].copy_from_slice(&e.ino.to_be_bytes());
        block[at + d::NAMELEN] = u8::try_from(namelen).map_err(|_| {
            Error::UnsupportedFeature(format!("a name of {namelen} bytes is too long"))
        })?;
        block[at + d::NAME..at + d::NAME + namelen].copy_from_slice(&e.name);
        block[at + d::NAME + namelen] = e.ftype;
        let tag = u16::try_from(at).expect("a directory block is at most 64 KiB");
        block[at + len - 2..at + len].copy_from_slice(&tag.to_be_bytes());
        let address = (db * dirblocksize as u64 + at as u64) / XFS_DIR2_DATA_ALIGN as u64;
        index.push((
            hash_for(sb, &e.name),
            u32::try_from(address).map_err(|_| {
                Error::UnsupportedFeature(format!(
                    "data block {db} lies past the addresses a directory index can hold"
                ))
            })?,
        ));
        at += len;
    }

    let free = dirblocksize - at;
    if free > 0 {
        use offsets::data_unused as u;
        block[at + u::FREETAG..at + u::FREETAG + 2]
            .copy_from_slice(&XFS_DIR2_DATA_FREE_TAG.to_be_bytes());
        let len = u16::try_from(free).expect("fits");
        block[at + u::LENGTH..at + u::LENGTH + 2].copy_from_slice(&len.to_be_bytes());
        let tag_at = at + u::tag(free);
        block[tag_at..tag_at + 2].copy_from_slice(&(at as u16).to_be_bytes());
        let bf = offsets::data_hdr::V5_BESTFREE;
        block[bf..bf + 2].copy_from_slice(&(at as u16).to_be_bytes());
        block[bf + 2..bf + 4].copy_from_slice(&len.to_be_bytes());
    }
    Ok(DataBlock {
        bytes: block,
        best: u16::try_from(free).expect("fits"),
        index,
    })
}

/// Build the leaf block of the directory `owner`, to live at filesystem
/// block `fsblock`, from every data block's index records and its
/// longest free region.
pub fn build_leaf(
    sb: &Superblock,
    fsblock: u64,
    owner: u64,
    mut index: Vec<(u32, u32)>,
    bests: &[u16],
) -> Result<Vec<u8>> {
    let dirblocksize = sb.dirblocksize() as usize;
    let capacity = leaf_capacity(dirblocksize, bests.len());
    if index.len() > capacity {
        return Err(Error::UnsupportedFeature(format!(
            "inode {owner}'s {} names need more index records than one leaf block holds \
             ({capacity}); that is the node form (#367)",
            index.len()
        )));
    }
    let mut block = vec![0u8; dirblocksize];
    use offsets::da_blk as b;
    block[b::MAGIC..b::MAGIC + 2].copy_from_slice(&XFS_DIR3_LEAF1_MAGIC.to_be_bytes());
    let blkno = crate::alloc_btree::blkno_of_fsbno(sb, fsblock);
    block[b::BLKNO..b::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
    block[b::UUID..b::UUID + 16].copy_from_slice(&sb.meta_uuid);
    block[b::OWNER..b::OWNER + 8].copy_from_slice(&owner.to_be_bytes());
    let count = u16::try_from(index.len()).expect("bounded by the capacity");
    let at = offsets::leaf_hdr::count(true);
    block[at..at + 2].copy_from_slice(&count.to_be_bytes());
    // Nothing is stale in an index that has just been built.

    // Sorted by hash; a stable sort keeps names that collide in the order
    // they were placed.
    index.sort_by_key(|&(hash, _)| hash);
    for (i, &(hash, address)) in index.iter().enumerate() {
        let at = XFS_DIR3_LEAF_HDR_SIZE + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        use offsets::leaf_entry as l;
        block[at + l::HASHVAL..at + l::HASHVAL + 4].copy_from_slice(&hash.to_be_bytes());
        block[at + l::ADDRESS..at + l::ADDRESS + 4].copy_from_slice(&address.to_be_bytes());
    }

    let start = offsets::leaf_tail::bests_start(dirblocksize, bests.len());
    for (i, best) in bests.iter().enumerate() {
        let at = start + i * XFS_DIR2_BEST_SIZE;
        block[at..at + 2].copy_from_slice(&best.to_be_bytes());
    }
    let tail = offsets::leaf_tail::at(dirblocksize);
    block[tail..tail + 4].copy_from_slice(&(bests.len() as u32).to_be_bytes());
    Ok(block)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(n: usize) -> Entry {
        Entry {
            name: format!("entry-number-{n:05}").into_bytes(),
            ino: 1000 + n as u64,
            ftype: 1,
        }
    }

    #[test]
    fn packing_fills_each_block_before_the_next() {
        let entries: Vec<Entry> = (0..400).map(named).collect();
        let blocks = pack(&entries, 4096).unwrap();
        assert!(blocks.len() > 1);
        let size = entry_size(named(0).name.len());
        let per = (4096 - XFS_DIR3_DATA_HDR_SIZE) / size;
        assert!(blocks[..blocks.len() - 1].iter().all(|b| b.len() == per));
        assert_eq!(blocks.iter().map(Vec::len).sum::<usize>(), 400);
    }

    #[test]
    fn a_leaf_has_room_for_fewer_records_as_bests_grow() {
        assert!(leaf_capacity(4096, 1) > leaf_capacity(4096, 100));
        assert_eq!(leaf_capacity(4096, 0), (4096 - 64 - 4) / 8);
    }
}
