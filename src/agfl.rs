//! The allocation group free list: blocks held aside for the group's
//! own trees.
//!
//! # Why a filesystem needs one
//!
//! Growing the free-space tree needs a block, and that block cannot come
//! out of the free-space tree, because taking it is the very edit that
//! needed the block. XFS breaks the recursion by keeping a handful of
//! blocks off to one side, in a ring in the group's fourth sector: a
//! tree that has to grow takes one from there, and the list is refilled
//! outside the edit that emptied it.
//!
//! Blocks on the list are not free space. `agf_freeblks` counts what the
//! free-space trees hold and nothing else, so moving a block between the
//! two changes both counts, and the superblock's own idea of free space
//! -- which counts the list as free -- does not move.
//!
//! # The ring
//!
//! `agf_flfirst` and `agf_fllast` index the entries, `agf_flcount` says
//! how many are live, and both indices wrap. Entries outside the live
//! span are whatever they were last time; a real filesystem's list is
//! full of stale block numbers below `flfirst`, which is why the count
//! is the authority and the array is not.

use crate::ag::{Agf, XFS_AGFL_MAGIC};
use crate::endian::{be32, uuid_at};
use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// Byte offsets in the v5 header. A v4 free list has no header at all
/// and its entries start at zero.
pub mod offsets {
    pub const MAGIC: usize = 0;
    pub const SEQNO: usize = 4;
    pub const UUID: usize = 8;
    pub const LSN: usize = 24;
    pub const CRC: usize = 32;
}

/// Where the entries start, after the v5 header.
pub const V5_HEADER_LEN: usize = 36;

/// One entry: a group-relative block number.
pub const ENTRY_LEN: usize = 4;

/// How many blocks the list can hold in a sector of `sectsize` bytes.
///
/// 119 at the 512-byte sector every fixture here uses.
pub fn capacity(sb: &Superblock) -> usize {
    let header = if sb.is_v5() { V5_HEADER_LEN } else { 0 };
    (usize::from(sb.sectsize) - header) / ENTRY_LEN
}

/// The free list of one group, read and checked.
#[derive(Debug, Clone)]
pub struct Agfl {
    /// The sector as it was read, kept so an edit can be diffed against
    /// it.
    raw: Vec<u8>,
    /// Index of the first live entry.
    first: u32,
    /// Index of the last live entry.
    last: u32,
    /// How many entries are live.
    count: u32,
    capacity: usize,
}

impl Agfl {
    /// Read the list out of the group's fourth sector, checking that it
    /// is the list this group's header describes.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] for a sector that is not a free list or
    /// whose indices do not describe the count the AGF states,
    /// [`Error::ChecksumMismatch`] and [`Error::BlockIdentityMismatch`]
    /// for a sector belonging to another filesystem or another group.
    pub fn parse(buf: &[u8], sb: &Superblock, agf: &Agf, agno: u32) -> Result<Self> {
        let capacity = capacity(sb);
        if buf.len() < usize::from(sb.sectsize) {
            return Err(Error::BadSuperblock(format!(
                "AG {agno}: the free list is {} bytes, shorter than a {}-byte sector",
                buf.len(),
                sb.sectsize
            )));
        }

        if sb.is_v5() {
            let magic = be32(buf, offsets::MAGIC);
            if magic != XFS_AGFL_MAGIC {
                return Err(Error::BadSuperblock(format!(
                    "AG {agno}: the free list has magic {magic:#010x}, expected \
                     {XFS_AGFL_MAGIC:#010x}"
                )));
            }
            if be32(buf, offsets::CRC).swap_bytes() != crc32c_with_zeroed_crc(buf, offsets::CRC) {
                return Err(Error::ChecksumMismatch {
                    what: "AGFL",
                    block: u64::from(agno),
                });
            }
            if uuid_at(buf, offsets::UUID) != sb.meta_uuid {
                return Err(Error::BlockIdentityMismatch {
                    what: "AGFL",
                    expected: u64::from(agno),
                    found: u64::MAX,
                });
            }
            let seqno = be32(buf, offsets::SEQNO);
            if seqno != agno {
                return Err(Error::BlockIdentityMismatch {
                    what: "AGFL",
                    expected: u64::from(agno),
                    found: u64::from(seqno),
                });
            }
        }

        // The count is the authority and the indices have to agree with
        // it. They can disagree only if the header is wrong, and a
        // wrong span hands out blocks that belong to something else.
        let span = if agf.flcount == 0 {
            0
        } else {
            (agf.fllast + capacity as u32 - agf.flfirst) % capacity as u32 + 1
        };
        if agf.flcount as usize > capacity || span != agf.flcount {
            return Err(Error::BadSuperblock(format!(
                "AG {agno}: the free list says {} blocks between entries {} and {}, which spans \
                 {span} of {capacity}",
                agf.flcount, agf.flfirst, agf.fllast
            )));
        }

        Ok(Agfl {
            raw: buf[..usize::from(sb.sectsize)].to_vec(),
            first: agf.flfirst,
            last: agf.fllast,
            count: agf.flcount,
            capacity,
        })
    }

    /// How many blocks are on the list.
    pub fn count(&self) -> u32 {
        self.count
    }

    /// Index of the first live entry.
    pub fn first(&self) -> u32 {
        self.first
    }

    /// Index of the last live entry.
    pub fn last(&self) -> u32 {
        self.last
    }

    fn entry_at(&self, sb: &Superblock, index: u32) -> usize {
        let header = if sb.is_v5() { V5_HEADER_LEN } else { 0 };
        header + index as usize * ENTRY_LEN
    }

    /// Take the oldest block off the list.
    ///
    /// Oldest rather than newest: the list is a queue, and a block that
    /// has just been freed onto it may still be referred to by a log
    /// record that has not been replayed. Handing back the one that has
    /// been there longest is what keeps that from mattering.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when the list is empty. Refilling
    /// it means taking blocks out of the free-space trees, which is the
    /// edit that wanted a block in the first place.
    pub fn take(&mut self, sb: &Superblock, agno: u32) -> Result<u32> {
        if self.count == 0 {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno}'s free list is empty, and refilling it is not \
                 implemented; the tree cannot grow here"
            )));
        }
        let at = self.entry_at(sb, self.first);
        let block = be32(&self.raw, at);
        self.first = (self.first + 1) % self.capacity as u32;
        self.count -= 1;
        Ok(block)
    }

    /// Put a block the trees no longer need back on the list.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] when the list is full, which means
    /// the block has to go back into free space instead.
    pub fn put(&mut self, sb: &Superblock, agno: u32, block: u32) -> Result<()> {
        if self.count as usize >= self.capacity {
            return Err(Error::UnsupportedFeature(format!(
                "allocation group {agno}'s free list is full at {} blocks, and returning one to \
                 free space instead is not implemented",
                self.capacity
            )));
        }
        // The first block onto an empty list goes at `first` rather
        // than after `last`, which is stale once the list has emptied.
        let index = if self.count == 0 {
            self.first
        } else {
            (self.last + 1) % self.capacity as u32
        };
        let at = self.entry_at(sb, index);
        self.raw[at..at + ENTRY_LEN].copy_from_slice(&block.to_be_bytes());
        self.last = index;
        if self.count == 0 {
            self.first = index;
        }
        self.count += 1;
        Ok(())
    }

    /// The sector as it was read, for a buffer item to diff against.
    pub fn before(&self) -> &[u8] {
        &self.raw
    }

    /// The sector as it stands now, checksummed.
    pub fn after(&self, sb: &Superblock) -> Vec<u8> {
        let mut out = self.raw.clone();
        if sb.is_v5() {
            let crc = crc32c_with_zeroed_crc(&out, offsets::CRC);
            out[offsets::CRC..offsets::CRC + 4].copy_from_slice(&crc.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECTSIZE: u16 = 512;

    fn sb() -> Superblock {
        let mut b = vec![0u8; SECTSIZE as usize];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&1024u32.to_be_bytes()); // blocksize
        b[8..16].copy_from_slice(&4096u64.to_be_bytes()); // dblocks
        for (i, slot) in b[32..48].iter_mut().enumerate() {
            *slot = i as u8;
        }
        b[48..56].copy_from_slice(&100u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&2048u32.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&2u32.to_be_bytes()); // agcount
        let versionnum = 5u16 | crate::superblock::version_flags::MOREBITSBIT;
        b[100..102].copy_from_slice(&versionnum.to_be_bytes());
        b[102..104].copy_from_slice(&SECTSIZE.to_be_bytes());
        b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        b[106..108].copy_from_slice(&2u16.to_be_bytes()); // inopblock
        b[120] = 10; // blocklog
        b[121] = 9; // sectlog
        b[122] = 9; // inodelog
        b[123] = 1; // inopblog
        b[124] = 11; // agblklog
        let crc = crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).expect("v5 superblock")
    }

    /// A list holding `blocks` from entry `first` onwards, and the AGF
    /// that describes it.
    fn list(sb: &Superblock, first: u32, blocks: &[u32]) -> (Vec<u8>, Agf) {
        let cap = capacity(sb) as u32;
        let mut raw = vec![0u8; SECTSIZE as usize];
        raw[offsets::MAGIC..offsets::MAGIC + 4].copy_from_slice(&XFS_AGFL_MAGIC.to_be_bytes());
        raw[offsets::SEQNO..offsets::SEQNO + 4].copy_from_slice(&0u32.to_be_bytes());
        raw[offsets::UUID..offsets::UUID + 16].copy_from_slice(&sb.meta_uuid);
        for (i, b) in blocks.iter().enumerate() {
            let index = (first + i as u32) % cap;
            let at = V5_HEADER_LEN + index as usize * ENTRY_LEN;
            raw[at..at + 4].copy_from_slice(&b.to_be_bytes());
        }
        let crc = crc32c_with_zeroed_crc(&raw, offsets::CRC);
        raw[offsets::CRC..offsets::CRC + 4].copy_from_slice(&crc.to_le_bytes());

        let mut agf = crate::ag::Agf {
            seqno: 0,
            length: 2048,
            roots: [1, 2, 0],
            levels: [1, 1, 0],
            flfirst: first,
            fllast: if blocks.is_empty() {
                first
            } else {
                (first + blocks.len() as u32 - 1) % cap
            },
            flcount: blocks.len() as u32,
            freeblks: 0,
            longest: 0,
            btreeblks: 0,
            rmap_blocks: 0,
            refcount_root: 0,
            refcount_level: 0,
        };
        if blocks.is_empty() {
            agf.fllast = first;
        }
        (raw, agf)
    }

    /// The measured shape: 512-byte sectors hold 119 entries.
    #[test]
    fn a_sector_holds_a_hundred_and_nineteen_blocks() {
        assert_eq!(capacity(&sb()), 119);
    }

    /// Taking hands back the oldest, in the order they went on.
    #[test]
    fn the_list_is_a_queue_rather_than_a_stack() {
        let sb = sb();
        let (raw, agf) = list(&sb, 7, &[960, 961, 962]);
        let mut fl = Agfl::parse(&raw, &sb, &agf, 0).expect("a legal list");

        assert_eq!(fl.take(&sb, 0).expect("take"), 960);
        assert_eq!(fl.take(&sb, 0).expect("take"), 961);
        assert_eq!(fl.count(), 1);
        fl.put(&sb, 0, 4242).expect("put");
        assert_eq!(fl.count(), 2);
        assert_eq!(fl.take(&sb, 0).expect("take"), 962, "962 was there first");
        assert_eq!(fl.take(&sb, 0).expect("take"), 4242);
        assert_eq!(fl.count(), 0);
    }

    /// The indices wrap, and a list that runs off the end of the sector
    /// continues at its start rather than writing past it.
    #[test]
    fn the_ring_wraps_at_the_end_of_the_sector() {
        let sb = sb();
        let cap = capacity(&sb) as u32;
        // Starting two from the end, so putting three crosses it.
        let (raw, agf) = list(&sb, cap - 2, &[10, 11]);
        let mut fl = Agfl::parse(&raw, &sb, &agf, 0).expect("a legal list");

        fl.put(&sb, 0, 12).expect("put");
        fl.put(&sb, 0, 13).expect("put");
        assert_eq!(fl.last(), 1, "the last entry wrapped to the start");
        assert_eq!(fl.take(&sb, 0).expect("take"), 10);
        assert_eq!(fl.take(&sb, 0).expect("take"), 11);
        assert_eq!(fl.take(&sb, 0).expect("take"), 12);
        assert_eq!(fl.take(&sb, 0).expect("take"), 13);
    }

    /// An empty list is refused rather than handing out whatever the
    /// stale entry at `flfirst` holds -- which on a real filesystem is a
    /// block number that belonged to something else.
    #[test]
    fn an_empty_list_hands_out_nothing() {
        let sb = sb();
        let (raw, agf) = list(&sb, 7, &[]);
        let mut fl = Agfl::parse(&raw, &sb, &agf, 0).expect("a legal list");
        let err = fl.take(&sb, 0).expect_err("nothing to take");
        assert!(format!("{err}").contains("free list is empty"), "{err}");
    }

    /// An emptied list starts again at `flfirst`, because `fllast` is
    /// stale once the two have crossed.
    #[test]
    fn putting_onto_an_emptied_list_starts_it_again() {
        let sb = sb();
        let (raw, agf) = list(&sb, 7, &[960]);
        let mut fl = Agfl::parse(&raw, &sb, &agf, 0).expect("a legal list");
        assert_eq!(fl.take(&sb, 0).expect("take"), 960);
        assert_eq!(fl.count(), 0);

        fl.put(&sb, 0, 77).expect("put");
        assert_eq!(fl.count(), 1);
        assert_eq!(fl.first(), fl.last(), "one block is both ends of the list");
        assert_eq!(fl.take(&sb, 0).expect("take"), 77);
    }

    /// A header whose count and indices disagree describes a span that
    /// is not there, and the blocks outside it belong to something else.
    #[test]
    fn a_count_the_indices_do_not_support_is_refused() {
        let sb = sb();
        let (raw, mut agf) = list(&sb, 7, &[960, 961, 962]);
        agf.flcount = 9;
        let err = Agfl::parse(&raw, &sb, &agf, 0).expect_err("the span is three, not nine");
        assert!(
            format!("{err}").contains("free list says 9 blocks"),
            "{err}"
        );
    }

    /// Rewriting the sector leaves everything but the entries and the
    /// checksum alone -- the header identifies the group and does not
    /// change when a block moves.
    #[test]
    fn writing_the_list_back_changes_only_what_moved() {
        let sb = sb();
        let (raw, agf) = list(&sb, 7, &[960, 961]);
        let mut fl = Agfl::parse(&raw, &sb, &agf, 0).expect("a legal list");
        fl.put(&sb, 0, 4242).expect("put");
        let after = fl.after(&sb);

        assert_eq!(
            &after[..offsets::CRC],
            &raw[..offsets::CRC],
            "the identifying header is untouched"
        );
        let at = V5_HEADER_LEN + 9 * ENTRY_LEN;
        assert_eq!(be32(&after, at), 4242, "the new block is where it belongs");
        assert_eq!(
            be32(&after, offsets::CRC).swap_bytes(),
            crc32c_with_zeroed_crc(&after, offsets::CRC),
            "and the sector checksums"
        );
    }
}
