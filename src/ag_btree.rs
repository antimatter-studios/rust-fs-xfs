//! The four short-form B+trees a group keeps, and one way to walk them.
//!
//! An allocation group carries a free-space tree by block, a second by
//! length, a reverse-mapping tree and a reference-count tree. All four
//! use the same block: the same header, the same self-describing
//! identity fields, keys packed forwards from the body and child
//! pointers packed backwards from the end of the maximum key array.
//! They differ in three numbers — the magic they carry, how wide a
//! record is, and how wide a key is — and in nothing else.
//!
//! So the descent lives here once, parameterised by those three, and
//! each tree supplies its own [`Shape`] and its own record decoder.
//! Writing it per tree is what let `bb_numrecs` go unchecked in three
//! places at once: the identity checks below are the ones that catch a
//! block belonging to another group, sitting at another depth, or read
//! from somewhere other than where it says it lives.

use crate::endian::{be16, be32, be64, le32, uuid_at};
use crate::error::{Error, Result};
use crate::superblock::{crc32c_with_zeroed_crc, Superblock};

/// The v4 short-form header: magic, level, record count and two
/// siblings.
pub const V4_HEADER_LEN: usize = 16;

/// The v5 short-form header, which adds the block's own address, a
/// sequence number, the filesystem UUID, the owning group and a
/// checksum.
pub const V5_HEADER_LEN: usize = 56;

/// A child pointer is an allocation-group block number.
pub const PTR_LEN: usize = 4;

/// A tree deeper than this is not a tree, and the bound stops a cycle
/// in a corrupt image from being walked forever.
pub const MAX_LEVELS: u16 = 9;

/// Byte offsets within the short-form block header.
pub mod offsets {
    pub const MAGIC: usize = 0;
    pub const LEVEL: usize = 4;
    pub const NUMRECS: usize = 6;
    pub const BLKNO: usize = 16;
    pub const UUID: usize = 32;
    pub const OWNER: usize = 48;
    pub const CRC: usize = 52;
}

/// What tells one of the group's trees from another.
#[derive(Debug, Clone, Copy)]
pub struct Shape {
    /// What to call it when reporting a problem.
    pub name: &'static str,
    /// The magic a v4 block carries, where the tree exists on v4 at
    /// all. The reverse-mapping and reference-count trees are v5
    /// features and have none.
    pub magic_v4: Option<u32>,
    /// The magic a v5 block carries.
    pub magic_v5: u32,
    /// How wide one record is, in a leaf.
    pub record_len: usize,
    /// How wide one key is, in a node. Not always the record's width:
    /// a reference-count record is twelve bytes and its key is four.
    pub key_len: usize,
}

/// A header that has been read and checked.
pub struct Node {
    pub level: u16,
    pub numrecs: u16,
    /// Where the records or keys begin.
    pub body: usize,
    /// How many records the block could hold.
    pub maxrecs: usize,
}

/// How many entries fit in `space` bytes when each one is `per` bytes
/// wide.
///
/// A leaf's entry is a record; an internal node's is a key and the
/// pointer that follows it, which is why the caller decides the width
/// rather than this. `per` cannot be zero for any tree the group keeps
/// -- every [`Shape`] states both widths -- but the floor is here so
/// that a shape added later with a zero in it reports a block with room
/// for nothing rather than dividing by zero.
pub fn maxrecs(space: usize, per: usize) -> usize {
    space / per.max(1)
}

/// Read and check one block of one of the group's trees.
///
/// `expect_level` is the level the parent said this child sits at and
/// `agno` the group the tree belongs to. Checking both is what makes
/// the descent self-verifying: a block that belongs to another group,
/// or sits at a different depth than its parent believed, is rejected
/// before its contents are read.
pub fn parse_block(
    buf: &[u8],
    sb: &Superblock,
    shape: Shape,
    agno: u32,
    agblock: u32,
    expect_level: u16,
) -> Result<Node> {
    let header = if sb.is_v5() {
        V5_HEADER_LEN
    } else {
        V4_HEADER_LEN
    };
    let what = shape.name;
    if buf.len() < header {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is {} bytes, shorter than its {header}-byte header",
            buf.len()
        )));
    }

    let want = if sb.is_v5() {
        shape.magic_v5
    } else {
        shape.magic_v4.ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "AG {agno}: a {what} is a v5 feature and this filesystem is v4"
            ))
        })?
    };
    let magic = be32(buf, offsets::MAGIC);
    if magic != want {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} has magic {magic:#010x}, expected {want:#010x}"
        )));
    }

    if sb.is_v5() {
        let stored = le32(buf, offsets::CRC);
        if stored != crc32c_with_zeroed_crc(buf, offsets::CRC) {
            return Err(Error::ChecksumMismatch {
                what: "group btree block",
                block: u64::from(agblock),
            });
        }
        if uuid_at(buf, offsets::UUID) != sb.meta_uuid {
            return Err(Error::BlockIdentityMismatch {
                what: "group btree block",
                expected: u64::from(agblock),
                found: u64::MAX, // a UUID mismatch says nothing about the address
            });
        }
        // The owner is the group. A block from a different group would
        // otherwise decode into entirely plausible records belonging to
        // somewhere else.
        let owner = be32(buf, offsets::OWNER);
        if owner != agno {
            return Err(Error::BlockIdentityMismatch {
                what: "group btree block owner",
                expected: u64::from(agno),
                found: u64::from(owner),
            });
        }
        // The block records its own address, so a block read from the
        // wrong place says so rather than being believed.
        let stated = be64(buf, offsets::BLKNO);
        let expected = crate::alloc_btree::expected_blkno(sb, agno, agblock);
        if stated != expected {
            return Err(Error::BlockIdentityMismatch {
                what: "group btree block address",
                expected,
                found: stated,
            });
        }
    }

    let level = be16(buf, offsets::LEVEL);
    if level != expect_level {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} is at level {level}, but its parent points to it \
             as level {expect_level}"
        )));
    }

    let space = buf.len() - header;
    let per = if level == 0 {
        shape.record_len
    } else {
        shape.key_len + PTR_LEN
    };
    let max = maxrecs(space, per);
    let numrecs = be16(buf, offsets::NUMRECS);
    if usize::from(numrecs) > max {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {what} block {agblock} claims {numrecs} records but has room for {max}"
        )));
    }

    Ok(Node {
        level,
        numrecs,
        body: header,
        maxrecs: max,
    })
}

/// Collect every record in one of a group's trees, in the order the
/// tree keeps them.
///
/// `read_agblock` fetches one block of the group by its group-relative
/// number; the walker never assumes a block is where a cache would put
/// it. `decode` turns one record's bytes into whatever the caller keeps.
///
/// # Errors
///
/// [`Error::BadSuperblock`] for a malformed block or an impossible
/// depth, [`Error::ChecksumMismatch`] and [`Error::BlockIdentityMismatch`]
/// for a block that is not the one that was asked for, and whatever
/// `read_agblock` returns.
pub fn walk<T, F, D>(
    sb: &Superblock,
    shape: Shape,
    agno: u32,
    root: u32,
    levels: u32,
    mut read_agblock: F,
    decode: D,
) -> Result<Vec<T>>
where
    F: FnMut(u32) -> Result<Vec<u8>>,
    D: Fn(&[u8], usize) -> T,
{
    if levels == 0 || levels > u32::from(MAX_LEVELS) {
        return Err(Error::BadSuperblock(format!(
            "AG {agno}: {} claims {levels} levels, which is not a tree",
            shape.name
        )));
    }

    let mut out = Vec::new();
    // Depth-first, left to right, so records arrive in the tree's own
    // order and a caller can check that ordering rather than impose it.
    let mut stack = vec![(root, (levels - 1) as u16)];
    // HOW MANY BLOCKS THE WALK MAY VISIT.
    //
    // `MAX_LEVELS` bounds how deep the tree goes and says nothing about
    // how wide it is, and the two are not the same bound. Nine blocks,
    // each at its own address, each stating a level one below its
    // parent's and pointing every one of its slots at the block below,
    // pass the magic, CRC, owner, level and self-address checks --
    // because each one genuinely is the block at its own address -- and
    // cost 336^8 visits at a 4 KiB block size. The record vector grows
    // per leaf, so it is memory exhaustion within seconds rather than a
    // pure hang.
    //
    // A tree inside an allocation group cannot have more blocks than
    // the group has.
    let mut budget = u64::from(sb.agblocks).max(64);

    while let Some((agblock, expect_level)) = stack.pop() {
        budget = budget.checked_sub(1).ok_or_else(|| {
            Error::BadSuperblock(format!(
                "AG {agno}: the walk visited more blocks than the group holds; the \
                 tree points back into itself"
            ))
        })?;
        let buf = read_agblock(agblock)?;
        let node = parse_block(&buf, sb, shape, agno, agblock, expect_level)?;

        if node.level == 0 {
            let end = node.body + usize::from(node.numrecs) * shape.record_len;
            if end > buf.len() {
                return Err(Error::BadSuperblock(format!(
                    "AG {agno}: {} leaf {agblock} needs {end} bytes for its {} records \
                     but is only {} long",
                    shape.name,
                    node.numrecs,
                    buf.len()
                )));
            }
            for i in 0..usize::from(node.numrecs) {
                out.push(decode(&buf, node.body + i * shape.record_len));
            }
            continue;
        }

        // The pointers start after room for the maximum number of keys,
        // not after the keys in use.
        let first = node.body + node.maxrecs * shape.key_len;
        let end = first + usize::from(node.numrecs) * PTR_LEN;
        if end > buf.len() {
            return Err(Error::BadSuperblock(format!(
                "AG {agno}: {} node {agblock} needs {end} bytes for its pointer array \
                 but is only {} long",
                shape.name,
                buf.len()
            )));
        }
        // Pushed in reverse so the leftmost child is visited first.
        for i in (0..usize::from(node.numrecs)).rev() {
            stack.push((be32(&buf, first + i * PTR_LEN), node.level - 1));
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two header sizes, which are the thing most likely to be got
    /// wrong by analogy with the block-map tree's 72-byte v5 header.
    ///
    /// Moved here with the constants when the four group trees stopped
    /// carrying a copy of the descent each.
    #[test]
    fn the_short_form_headers_are_smaller_than_the_long_form() {
        assert_eq!(V4_HEADER_LEN, 16);
        assert_eq!(V5_HEADER_LEN, 56);
        // Where the v5 header's fields land, as read off a real root.
        assert_eq!(offsets::BLKNO + 8, 24);
        assert_eq!(offsets::UUID + 16, offsets::OWNER);
        assert_eq!(offsets::OWNER + 4, offsets::CRC);
        assert_eq!(offsets::CRC + 4, V5_HEADER_LEN);
    }

    /// The four trees differ in three numbers and nothing else, and
    /// these are the numbers. A key is not always the width of the
    /// record it indexes: a reference-count record is twelve bytes and
    /// its key is the four-byte start block.
    #[test]
    fn each_tree_is_told_from_the_others_by_its_magic_and_its_widths() {
        let bno = crate::alloc_btree::Order::ByBlock.shape();
        assert_eq!(&bno.magic_v5.to_be_bytes(), b"AB3B");
        assert_eq!((bno.record_len, bno.key_len), (8, 8));

        let rmap = crate::rmap::shape();
        assert_eq!(&rmap.magic_v5.to_be_bytes(), b"RMB3");
        assert_eq!((rmap.record_len, rmap.key_len), (24, 20));

        let refcount = crate::refcount::shape();
        assert_eq!(&refcount.magic_v5.to_be_bytes(), b"R3FC");
        assert_eq!((refcount.record_len, refcount.key_len), (12, 4));

        // Both are v5 features, so a v4 filesystem has neither -- and
        // saying so by name beats reading a v4 block as one.
        assert!(rmap.magic_v4.is_none());
        assert!(refcount.magic_v4.is_none());
        assert!(bno.magic_v4.is_some());
    }

    // -----------------------------------------------------------------
    // Walking a tree that is more than one block
    // -----------------------------------------------------------------

    /// A v5 superblock whose groups are NOT a power of two blocks long,
    /// so a block's self-address distinguishes a packed pointer from a
    /// linear block number instead of coinciding with it.
    fn v5_superblock() -> Superblock {
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
        b[8..16].copy_from_slice(&4000u64.to_be_bytes()); // dblocks
        b[48..56].copy_from_slice(&100u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&1000u32.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
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
        for (i, slot) in b[32..48].iter_mut().enumerate() {
            *slot = i as u8;
        }
        let crc = crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).expect("v5 superblock")
    }

    /// The by-block free-space tree, whose record and key are both eight
    /// bytes -- the simplest of the four to build by hand.
    fn bno() -> Shape {
        crate::alloc_btree::Order::ByBlock.shape()
    }

    /// Stamp the header every group-tree block carries, and checksum it.
    fn stamp(buf: &mut [u8], sb: &Superblock, agno: u32, agblock: u32, level: u16, numrecs: u16) {
        buf[offsets::MAGIC..offsets::MAGIC + 4].copy_from_slice(&bno().magic_v5.to_be_bytes());
        buf[offsets::LEVEL..offsets::LEVEL + 2].copy_from_slice(&level.to_be_bytes());
        buf[offsets::NUMRECS..offsets::NUMRECS + 2].copy_from_slice(&numrecs.to_be_bytes());
        let blkno = crate::alloc_btree::expected_blkno(sb, agno, agblock);
        buf[offsets::BLKNO..offsets::BLKNO + 8].copy_from_slice(&blkno.to_be_bytes());
        buf[offsets::UUID..offsets::UUID + 16].copy_from_slice(&sb.meta_uuid);
        buf[offsets::OWNER..offsets::OWNER + 4].copy_from_slice(&agno.to_be_bytes());
        let crc = crc32c_with_zeroed_crc(buf, offsets::CRC);
        buf[offsets::CRC..offsets::CRC + 4].copy_from_slice(&crc.to_le_bytes());
    }

    /// A leaf holding `runs` as (start, length) pairs.
    fn leaf(sb: &Superblock, agno: u32, agblock: u32, runs: &[(u32, u32)]) -> Vec<u8> {
        let mut buf = vec![0u8; sb.blocksize as usize];
        for (i, (start, len)) in runs.iter().enumerate() {
            let at = V5_HEADER_LEN + i * bno().record_len;
            buf[at..at + 4].copy_from_slice(&start.to_be_bytes());
            buf[at + 4..at + 8].copy_from_slice(&len.to_be_bytes());
        }
        stamp(&mut buf, sb, agno, agblock, 0, runs.len() as u16);
        buf
    }

    /// An internal node at `level`, pointing at `children`.
    ///
    /// The pointer array starts after room for the MAXIMUM number of
    /// keys, not after the keys in use -- getting that wrong reads a
    /// pointer out of the middle of the key array and is the reason this
    /// is built here rather than assumed.
    fn node(
        sb: &Superblock,
        agno: u32,
        agblock: u32,
        level: u16,
        children: &[(u32, u32)],
    ) -> Vec<u8> {
        let mut buf = vec![0u8; sb.blocksize as usize];
        let shape = bno();
        let maxrecs = maxrecs(
            sb.blocksize as usize - V5_HEADER_LEN,
            shape.key_len + PTR_LEN,
        );
        let ptrs = V5_HEADER_LEN + maxrecs * shape.key_len;
        for (i, (first_key, child)) in children.iter().enumerate() {
            let key_at = V5_HEADER_LEN + i * shape.key_len;
            buf[key_at..key_at + 4].copy_from_slice(&first_key.to_be_bytes());
            let ptr_at = ptrs + i * PTR_LEN;
            buf[ptr_at..ptr_at + 4].copy_from_slice(&child.to_be_bytes());
        }
        stamp(&mut buf, sb, agno, agblock, level, children.len() as u16);
        buf
    }

    fn decode_run(buf: &[u8], at: usize) -> (u32, u32) {
        (be32(buf, at), be32(buf, at + 4))
    }

    /// The point of the whole module: a tree of a root over two leaves
    /// gives up every record, in the tree's own order.
    ///
    /// Reading the root alone -- which is what each of the four trees
    /// did before -- returns the KEYS of a node as though they were
    /// records, so this is the difference between 6 runs and 2 wrong
    /// ones.
    #[test]
    fn a_two_level_tree_yields_every_record_in_order() {
        let sb = v5_superblock();
        let blocks = std::collections::HashMap::from([
            (1u32, node(&sb, 0, 1, 1, &[(10, 2), (700, 3)])),
            (2u32, leaf(&sb, 0, 2, &[(10, 6), (280, 8), (536, 4)])),
            (3u32, leaf(&sb, 0, 3, &[(700, 12), (900, 2), (950, 5)])),
        ]);

        let out = walk(&sb, bno(), 0, 1, 2, |b| Ok(blocks[&b].clone()), decode_run)
            .expect("a two-level tree walks");

        assert_eq!(
            out,
            vec![(10, 6), (280, 8), (536, 4), (700, 12), (900, 2), (950, 5)],
            "left to right, leaf by leaf"
        );
    }

    /// A single-level tree is the root and nothing else, which is the
    /// shape every one of these trees had to be before.
    #[test]
    fn a_one_level_tree_is_read_from_its_root() {
        let sb = v5_superblock();
        let root = leaf(&sb, 0, 1, &[(10, 6), (280, 8)]);
        let out = walk(&sb, bno(), 0, 1, 1, |_| Ok(root.clone()), decode_run)
            .expect("a one-level tree walks");
        assert_eq!(out, vec![(10, 6), (280, 8)]);
    }

    /// The level a child states has to be the one its parent points to
    /// it as. Without it a cycle back to the root reads as a deeper
    /// tree rather than as the loop it is.
    #[test]
    fn a_child_that_disagrees_about_its_level_is_refused() {
        let sb = v5_superblock();
        let blocks = std::collections::HashMap::from([
            (1u32, node(&sb, 0, 1, 1, &[(10, 2)])),
            // The parent points to this as a leaf; it says it is a node.
            (2u32, node(&sb, 0, 2, 1, &[(10, 3)])),
        ]);
        let err = walk(&sb, bno(), 0, 1, 2, |b| Ok(blocks[&b].clone()), decode_run)
            .expect_err("a child at the wrong level is not read");
        assert!(
            format!("{err}").contains("level"),
            "the message should name the disagreement: {err}"
        );
    }

    /// A block belonging to another group decodes into entirely
    /// plausible records, which is why the owner is checked rather than
    /// trusted.
    #[test]
    fn a_block_owned_by_another_group_is_refused() {
        let sb = v5_superblock();
        let blocks = std::collections::HashMap::from([
            (1u32, node(&sb, 0, 1, 1, &[(10, 2)])),
            (2u32, leaf(&sb, 1, 2, &[(10, 6)])),
        ]);
        let err = walk(&sb, bno(), 0, 1, 2, |b| Ok(blocks[&b].clone()), decode_run)
            .expect_err("a block from another group is not read");
        assert!(matches!(err, Error::BlockIdentityMismatch { .. }), "{err}");
    }

    /// Nine levels, each block pointing at eight children, is a
    /// filesystem no image has and a walk no budget-free descent
    /// survives.
    ///
    /// Every block here passes every identity check -- it genuinely is
    /// the block at its own address, stating a level one below its
    /// parent's -- so nothing but the visit budget stops it. Eight
    /// children per node over nine levels is ~19 million visits, and
    /// the record vector grows per leaf, so unbounded it is memory
    /// exhaustion rather than a hang anyone would see as one.
    #[test]
    fn a_tree_wider_than_the_group_is_stopped() {
        let sb = v5_superblock();
        let mut blocks = std::collections::HashMap::new();
        blocks.insert(1u32, leaf(&sb, 0, 1, &[(10, 6)]));
        for b in 2u32..=9 {
            let children: Vec<(u32, u32)> = (0..8).map(|i| (10 * i, b - 1)).collect();
            blocks.insert(b, node(&sb, 0, b, (b - 1) as u16, &children));
        }

        let err = walk(&sb, bno(), 0, 9, 9, |b| Ok(blocks[&b].clone()), decode_run)
            .expect_err("a walk wider than the group is stopped");
        assert!(
            format!("{err}").contains("more blocks than the group holds"),
            "{err}"
        );
    }

    /// A depth no tree has is refused before a block is read, so a
    /// corrupt AGF cannot make the walk allocate its way through nine
    /// levels of nothing.
    #[test]
    fn an_impossible_depth_is_refused_before_anything_is_read() {
        let sb = v5_superblock();
        for levels in [0, u32::from(MAX_LEVELS) + 1] {
            let err = walk(
                &sb,
                bno(),
                0,
                1,
                levels,
                |_| panic!("no block should be read"),
                decode_run,
            )
            .expect_err("an impossible depth is refused");
            assert!(format!("{err}").contains("not a tree"), "{err}");
        }
    }
}
