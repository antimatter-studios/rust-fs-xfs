//! The reference-count B+tree: which extents more than one file holds.
//!
//! `reflink` lets two files point at the same blocks. The refcount tree
//! is what stops the second one being surprised when the first is
//! deleted: it holds a record per shared extent saying how many owners
//! it has, and blocks go back to free space only when the last one lets
//! go.
//!
//! # Why a driver that writes has to care
//!
//! Freeing returns blocks to the group's free space. If another file
//! still points at them, the allocator will hand them out again and the
//! two files will overwrite each other. `xfs_repair`, after exactly
//! that:
//!
//! ```text
//! data fork in ino 134 claims free block 24
//! ```
//!
//! # What the kernel does, measured
//!
//! An 8-block file at group block 24, then `cp --reflink=always`, then
//! truncating each copy in turn. Read back with `xfs_db`:
//!
//! ```text
//! one file          (no record — an unshared extent has none)
//! after the copy    [24,8,2,0]        startblock, blockcount, refcount, cowflag
//! first truncated   (no record)       and the free space is UNCHANGED
//! second truncated  free space gains  [24,25576]
//! ```
//!
//! Three things follow, and all three matter:
//!
//! 1. an extent with one owner has no record at all, so a missing record
//!    means unshared rather than unknown;
//! 2. freeing a shared extent decrements, and at one owner the record
//!    goes — the blocks stay put;
//! 3. only the last owner's free returns the blocks.

use crate::error::{Error, Result};
use crate::group_write::btree;

/// `sizeof(struct xfs_refcount_rec)`.
pub const RECORD: usize = 12;

/// `XFS_REFC_COWFLAG` — the record describes a copy-on-write staging
/// extent rather than a shared one. They live in the same tree, above
/// the ordinary records, and are not something this driver produces.
pub const COW_FLAG: u32 = 1 << 31;

/// One record: an extent, and how many files hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refcount {
    /// Group-relative. The top bit is the COW flag, kept out of here and
    /// in `cow` so the block number is a block number.
    pub startblock: u32,
    pub blockcount: u32,
    pub refcount: u32,
    pub cow: bool,
}

/// `R3FC` — the reference-count tree. A v5 feature, so there is no v4
/// magic to pair it with.
pub const XFS_REFC_CRC_MAGIC: u32 = 0x5233_4643;

/// A key is the record's start block alone: four bytes, where a record
/// is twelve.
const KEY: usize = 4;

/// What tells this tree from the group's other three.
pub fn shape() -> crate::ag_btree::Shape {
    crate::ag_btree::Shape {
        name: "refcountbt",
        magic_v4: None,
        magic_v5: XFS_REFC_CRC_MAGIC,
        record_len: RECORD,
        key_len: KEY,
    }
}

/// One record, decoded from `at` bytes into `buf`.
pub fn decode(buf: &[u8], at: usize) -> Refcount {
    let raw = u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes"));
    Refcount {
        startblock: raw & !COW_FLAG,
        blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
        refcount: u32::from_be_bytes(buf[at + 8..at + 12].try_into().expect("4 bytes")),
        cow: raw & COW_FLAG != 0,
    }
}

/// Every reference-count record in a group, however deep its tree.
pub fn walk<F>(
    sb: &crate::superblock::Superblock,
    agno: u32,
    root: u32,
    levels: u32,
    read_agblock: F,
) -> Result<Vec<Refcount>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
{
    crate::ag_btree::walk(sb, shape(), agno, root, levels, read_agblock, decode)
}

/// The records of a single-level tree, read straight out of its root.
pub fn leaf_records(buf: &[u8], numrecs: u16) -> Vec<Refcount> {
    // A backstop: the count comes from `group_write::leaf_numrecs`,
    // which has already checked it against the block.
    let fit = buf.len().saturating_sub(btree::V5_BODY) / RECORD;
    (0..usize::from(numrecs).min(fit))
        .map(|i| {
            let at = btree::V5_BODY + i * RECORD;
            let raw = u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes"));
            Refcount {
                startblock: raw & !COW_FLAG,
                blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
                refcount: u32::from_be_bytes(buf[at + 8..at + 12].try_into().expect("4 bytes")),
                cow: raw & COW_FLAG != 0,
            }
        })
        .collect()
}

/// A tree root rewritten to hold `records`, with its count brought up to
/// date.
pub fn rebuild_leaf(original: &[u8], records: &[Refcount]) -> Vec<u8> {
    let mut out = original.to_vec();
    out[btree::NUMRECS..btree::NUMRECS + 2].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (i, r) in records.iter().enumerate() {
        let at = btree::V5_BODY + i * RECORD;
        let start = if r.cow {
            r.startblock | COW_FLAG
        } else {
            r.startblock
        };
        out[at..at + 4].copy_from_slice(&start.to_be_bytes());
        out[at + 4..at + 8].copy_from_slice(&r.blockcount.to_be_bytes());
        out[at + 8..at + 12].copy_from_slice(&r.refcount.to_be_bytes());
    }
    // The checksum is deliberately left stale; recovery recomputes it.
    out
}

/// How many records a v5 tree root of this block size can hold.
pub fn capacity(blocksize: u32) -> usize {
    (blocksize as usize - btree::V5_BODY) / RECORD
}

/// Give up one reference to `startblock..+blockcount`, and say which of
/// those blocks may go back to free space.
///
/// # The shape of the answer
///
/// Not a yes or no. One extent can meet the reference-count tree in
/// several places at once — parts shared with another file, parts not —
/// and each part has a different answer. So this returns the sub-ranges
/// that are nobody else's, which is what the caller frees.
///
/// A block a record covers is **never** freed here. Dropping from two
/// owners to one does not release it; it leaves it with whoever remains.
///
/// # Measured
///
/// Two files sharing sixteen blocks, then the middle four of one copy
/// overwritten so they stop being shared:
///
/// ```text
/// both share 16 blocks   [24,16,2,0]
/// after the overwrite    [24,6,2,0]  [34,6,2,0]
/// ```
///
/// The record SPLIT, and the middle simply stopped having one — an
/// extent with a single owner carries no record. Freeing that copy then
/// returns only those middle blocks, because the outer twelve are still
/// the other file's.
///
/// # What this refuses
///
/// A copy-on-write staging record over the blocks being freed. This
/// driver does not do copy-on-write, so one being there means something
/// else is going on and guessing would be worse than stopping.
pub fn release(
    records: &mut Vec<Refcount>,
    startblock: u32,
    blockcount: u32,
) -> Result<Vec<crate::alloc_btree::FreeExtent>> {
    let start = u64::from(startblock);
    let end = start + u64::from(blockcount);

    if let Some(cow) = records
        .iter()
        .find(|r| r.cow && overlaps(r, startblock, end))
    {
        return Err(Error::UnsupportedFeature(format!(
            "group blocks {startblock}..{end} overlap a copy-on-write staging record at \
             {}..{}; this driver does not do copy-on-write and will not free underneath one",
            cow.startblock,
            u64::from(cow.startblock) + u64::from(cow.blockcount)
        )));
    }

    // Everything this extent touches, in block order, so the gaps
    // between them are the blocks nobody else holds.
    let mut touched: Vec<usize> = records
        .iter()
        .enumerate()
        .filter(|(_, r)| !r.cow && overlaps(r, startblock, end))
        .map(|(i, _)| i)
        .collect();
    touched.sort_by_key(|&i| records[i].startblock);

    let mut freeable = Vec::new();
    let mut at = start;
    for &i in &touched {
        let r = records[i];
        let rs = u64::from(r.startblock);
        if rs > at {
            freeable.push(crate::alloc_btree::FreeExtent {
                startblock: at as u32,
                blockcount: (rs - at) as u32,
            });
        }
        at = at.max(u64::from(r.startblock) + u64::from(r.blockcount));
    }
    if at < end {
        freeable.push(crate::alloc_btree::FreeExtent {
            startblock: at as u32,
            blockcount: (end - at) as u32,
        });
    }

    // Now rewrite each record it touched. Done last, and by index from
    // the back, so the earlier reads are not disturbed by the edits.
    for &i in touched.iter().rev() {
        let r = records[i];
        if r.refcount < 2 {
            return Err(Error::UnsupportedFeature(format!(
                "the reference-count record for group blocks {}..{} says {} owners, and a \
                 record should only exist while there is more than one",
                r.startblock,
                u64::from(r.startblock) + u64::from(r.blockcount),
                r.refcount
            )));
        }
        let rs = u64::from(r.startblock);
        let re = rs + u64::from(r.blockcount);
        let hit_start = rs.max(start);
        let hit_end = re.min(end);

        // What replaces it: the part before, the part this extent let go
        // of, and the part after. The middle keeps a record only while
        // more than one owner is left.
        let mut pieces = Vec::with_capacity(3);
        if rs < hit_start {
            pieces.push(Refcount {
                startblock: rs as u32,
                blockcount: (hit_start - rs) as u32,
                refcount: r.refcount,
                cow: false,
            });
        }
        if r.refcount > 2 {
            pieces.push(Refcount {
                startblock: hit_start as u32,
                blockcount: (hit_end - hit_start) as u32,
                refcount: r.refcount - 1,
                cow: false,
            });
        }
        if hit_end < re {
            pieces.push(Refcount {
                startblock: hit_end as u32,
                blockcount: (re - hit_end) as u32,
                refcount: r.refcount,
                cow: false,
            });
        }

        records.splice(i..=i, pieces);
    }

    merge_adjacent(records);

    Ok(freeable)
}

/// Join records that touch and say the same thing.
///
/// Splitting a record leaves neighbours that are contiguous and carry
/// the same count -- three files sharing 32 blocks, one of them letting
/// go of the two ends, leaves `[24,12,2] [36,8,2] [44,12,2]` where the
/// tree should hold `[24,32,2]`. Both forms describe the same sharing,
/// and only one of them is the tree the kernel keeps: `xfs_repair`
/// reports the other as "record N in block (0/4) of refcount tree
/// should be merged with previous record", and then reads the first
/// record's length as the whole shared run -- "saw (0/24) len 12 ...
/// should be (0/24) len 32".
///
/// A copy-on-write staging record never merges with an ordinary one:
/// they describe different things over the same blocks, which is the
/// reason the flag exists.
fn merge_adjacent(records: &mut Vec<Refcount>) {
    records.sort_by_key(|r| (r.startblock, r.cow));
    let mut i = 0;
    while i + 1 < records.len() {
        let a = records[i];
        let b = records[i + 1];
        let adjoins = u64::from(a.startblock) + u64::from(a.blockcount) == u64::from(b.startblock);
        if adjoins && a.refcount == b.refcount && a.cow == b.cow {
            records[i].blockcount = a.blockcount + b.blockcount;
            records.remove(i + 1);
        } else {
            i += 1;
        }
    }
}

fn overlaps(r: &Refcount, startblock: u32, end: u64) -> bool {
    let rec_end = u64::from(r.startblock) + u64::from(r.blockcount);
    u64::from(r.startblock) < end && u64::from(startblock) < rec_end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The record the kernel wrote after `cp --reflink=always` of an
    /// 8-block file at group block 24.
    fn shared() -> Vec<Refcount> {
        vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 2,
            cow: false,
        }]
    }

    #[test]
    fn a_leaf_survives_being_read_and_written_back() {
        let want = shared();
        let block = rebuild_leaf(&vec![0u8; 4096], &want);
        assert_eq!(leaf_records(&block, 1), want);
    }

    /// The COW flag rides in the top bit of the start block, so a record
    /// carrying it must not come back with an enormous block number.
    #[test]
    fn the_cow_flag_is_not_part_of_the_block_number() {
        let staging = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: true,
        }];
        let block = rebuild_leaf(&vec![0u8; 4096], &staging);
        let read = leaf_records(&block, 1);
        assert_eq!(read[0].startblock, 24, "the block number is the low bits");
        assert!(read[0].cow, "and the flag is kept, not folded in");
        assert_eq!(read, staging);
    }

    fn ranges(v: &[crate::alloc_btree::FreeExtent]) -> Vec<(u32, u32)> {
        v.iter().map(|e| (e.startblock, e.blockcount)).collect()
    }

    /// An extent nothing shares has no record, and letting go of it
    /// frees all of it.
    #[test]
    fn an_unshared_extent_frees_entirely() {
        let mut records = Vec::new();
        let freed = release(&mut records, 24, 8).expect("release");
        assert_eq!(ranges(&freed), [(24, 8)]);
    }

    /// The measured sequence: two owners, then one, then none.
    #[test]
    fn the_last_owner_is_the_one_that_frees() {
        let mut records = shared();

        // The first file lets go: the record goes with it and NOTHING is
        // freed, because the second file still points at the blocks.
        let freed = release(&mut records, 24, 8).expect("release");
        assert!(
            freed.is_empty(),
            "two owners becoming one must free nothing, got {:?}",
            ranges(&freed)
        );
        assert!(records.is_empty(), "an unshared extent keeps no record");

        // The second file lets go: nothing holds them now.
        let freed = release(&mut records, 24, 8).expect("release");
        assert_eq!(ranges(&freed), [(24, 8)], "the last owner frees");
    }

    /// Three owners decrement rather than dropping the record.
    #[test]
    fn a_third_owner_leaves_the_record_behind() {
        let mut records = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 3,
            cow: false,
        }];
        let freed = release(&mut records, 24, 8).expect("release");
        assert!(freed.is_empty(), "still two owners, so nothing is freed");
        assert_eq!(records[0].refcount, 2, "one fewer owner, and still shared");
    }

    /// What the tree has to look like afterwards, not just what it has
    /// to say.
    ///
    /// Three files sharing 32 blocks at group block 24, with the middle
    /// eight of one copy overwritten, is `[24,12,3] [36,8,2] [44,12,3]`.
    /// The overwritten copy is then truncated, which lets go of the two
    /// shared ends and leaves every one of the 32 blocks held by exactly
    /// two files -- ONE record, not three.
    ///
    /// Three records saying the same thing describe the same sharing and
    /// are still wrong: `xfs_repair` reports "record 1 in block (0/4) of
    /// refcount tree should be merged with previous record", and then
    /// reads the first record as the whole run -- "Incorrect reference
    /// count: saw (0/24) len 12 nlinks 2; should be (0/24) len 32".
    /// Found by the feature matrix, on the reflink and reflink-finobt
    /// images, before this merged anything.
    #[test]
    fn letting_go_leaves_one_record_where_the_sharing_is_the_same() {
        let mut records = vec![
            Refcount {
                startblock: 24,
                blockcount: 12,
                refcount: 3,
                cow: false,
            },
            Refcount {
                startblock: 36,
                blockcount: 8,
                refcount: 2,
                cow: false,
            },
            Refcount {
                startblock: 44,
                blockcount: 12,
                refcount: 3,
                cow: false,
            },
        ];

        // The truncated file's two shared extents, freed one at a time,
        // the way `truncate` walks a fork.
        assert!(release(&mut records, 24, 12).expect("release").is_empty());
        assert!(release(&mut records, 44, 12).expect("release").is_empty());

        assert_eq!(
            records,
            vec![Refcount {
                startblock: 24,
                blockcount: 32,
                refcount: 2,
                cow: false
            }],
            "32 blocks held by two files are one record"
        );
    }

    /// A staging record describes something else over the same blocks,
    /// so it never merges with an ordinary one however well they adjoin.
    #[test]
    fn a_staging_record_does_not_merge_with_an_ordinary_one() {
        let mut records = vec![
            Refcount {
                startblock: 24,
                blockcount: 4,
                refcount: 2,
                cow: false,
            },
            Refcount {
                startblock: 28,
                blockcount: 4,
                refcount: 2,
                cow: true,
            },
        ];
        merge_adjacent(&mut records);
        assert_eq!(records.len(), 2, "the flag keeps them apart: {records:?}");
    }

    /// Letting go of the middle of a shared run splits the record in
    /// two, and frees nothing.
    ///
    /// Measured. Sixteen blocks shared by two files, then the middle
    /// four of one copy overwritten:
    ///
    /// ```text
    /// before   [24,16,2,0]
    /// after    [24,6,2,0]  [34,6,2,0]
    /// ```
    #[test]
    fn letting_go_of_the_middle_splits_the_record() {
        let mut records = vec![Refcount {
            startblock: 24,
            blockcount: 16,
            refcount: 2,
            cow: false,
        }];
        let freed = release(&mut records, 30, 4).expect("release");

        assert!(freed.is_empty(), "the other file still holds them");
        assert_eq!(
            records
                .iter()
                .map(|r| (r.startblock, r.blockcount, r.refcount))
                .collect::<Vec<_>>(),
            [(24, 6, 2), (34, 6, 2)],
            "the run either side stays shared, as the kernel wrote it"
        );
    }

    /// AN EXTENT PART SHARED AND PART NOT, which is what the file left
    /// behind by that overwrite looks like when it is truncated.
    ///
    /// Blocks 24..40, with 24..30 and 34..40 shared and 30..34 this
    /// file's alone. Only the middle may go back to free space.
    #[test]
    fn only_the_blocks_nobody_else_holds_are_freed() {
        let mut records = vec![
            Refcount {
                startblock: 24,
                blockcount: 6,
                refcount: 2,
                cow: false,
            },
            Refcount {
                startblock: 34,
                blockcount: 6,
                refcount: 2,
                cow: false,
            },
        ];
        let freed = release(&mut records, 24, 16).expect("release");

        assert_eq!(
            ranges(&freed),
            [(30, 4)],
            "the gap between the two shared runs, and nothing else"
        );
        assert!(
            records.is_empty(),
            "both runs had two owners and now have one, so neither keeps a record"
        );
    }

    /// A record wider than the extent keeps what the extent did not
    /// touch.
    #[test]
    fn a_record_wider_than_the_extent_keeps_its_edges() {
        let mut records = vec![Refcount {
            startblock: 20,
            blockcount: 20,
            refcount: 3,
            cow: false,
        }];
        let freed = release(&mut records, 24, 8).expect("release");

        assert!(freed.is_empty(), "two owners are left, so nothing is freed");
        assert_eq!(
            records
                .iter()
                .map(|r| (r.startblock, r.blockcount, r.refcount))
                .collect::<Vec<_>>(),
            [(20, 4, 3), (24, 8, 2), (32, 8, 3)],
            "three pieces: untouched, one owner fewer, untouched"
        );
    }

    #[test]
    fn what_is_not_implemented_is_refused_rather_than_guessed() {
        // A staging extent means copy-on-write, which this does not do.
        let mut staging = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: true,
        }];
        let err = release(&mut staging, 24, 8).unwrap_err();
        assert!(format!("{err}").contains("copy-on-write"), "got: {err}");

        // A record claiming one owner should not exist.
        let mut bad = vec![Refcount {
            startblock: 24,
            blockcount: 8,
            refcount: 1,
            cow: false,
        }];
        let err = release(&mut bad, 24, 8).unwrap_err();
        assert!(format!("{err}").contains("more than one"), "got: {err}");
    }
}
