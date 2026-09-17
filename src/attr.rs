//! Reading extended attributes (#91).
//!
//! [`crate::format::attr`] names every on-disk structure; this reads them.
//! Which shape an inode's attribute fork holds is `di_aformat` together with
//! the magic of fork block 0, as that module's header explains:
//!
//! - **local**: `xfs_attr_shortform` inline in the fork;
//! - **extents / btree**: the fork maps blocks, and block 0 is either the one
//!   leaf or the root of a node B-tree over leaves chained by `forw`.
//!
//! Leaf entries are local (value after the name in the leaf) or remote
//! (value in separate fork blocks, each with a 56-byte header on v5).
//!
//! Names are returned with their namespace prefix -- `user.`, `trusted.`,
//! `security.` -- which the disk does not store. An entry still flagged
//! incomplete (its value never finished writing) is not returned, and
//! neither is one whose flags claim a namespace this format does not define.

use crate::endian::{be16, be32};
use crate::error::{Error, Result};
use crate::extent::{self, Extent};
use crate::format::attr::{
    flags, leaf_hdr_size, node_hdr_size, offsets, rmt_blocks, XFS_ATTR3_LEAF_MAGIC,
    XFS_ATTR3_RMT_HDR_SIZE, XFS_ATTR_LEAF_ENTRY_SIZE, XFS_ATTR_LEAF_MAGIC,
    XFS_ATTR_SF_ENTRY_HDR_SIZE, XFS_ATTR_SF_HDR_SIZE, XFS_DA3_NODE_MAGIC, XFS_DA_NODE_MAGIC,
    XFS_DA_NODE_MAXDEPTH,
};
use crate::fs::Filesystem;
use crate::inode::{Format, Inode};

/// One extended attribute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Xattr {
    /// The full name, namespace prefix included (`user.colour`).
    pub name: Vec<u8>,
    /// The value, which may be empty.
    pub value: Vec<u8>,
}

/// The flags bits this reader understands. An entry carrying any other
/// bit -- a namespace added after this format was written down, such as
/// parent pointers -- is not an attribute to report under `user.`.
const KNOWN_FLAGS: u8 = flags::LOCAL | flags::ROOT | flags::SECURE | flags::INCOMPLETE;

fn corrupt(ino: u64, what: &str) -> Error {
    Error::BadSuperblock(format!("inode {ino}: attribute fork: {what}"))
}

/// The reported name, or `None` for an entry that is not reported.
fn full_name(ino: u64, entry_flags: u8, name: &[u8]) -> Result<Option<Vec<u8>>> {
    if entry_flags & flags::INCOMPLETE != 0 || entry_flags & !KNOWN_FLAGS != 0 {
        return Ok(None);
    }
    let prefix = flags::namespace_prefix(entry_flags)
        .ok_or_else(|| corrupt(ino, "an entry claims two namespaces"))?;
    let mut out = prefix.as_bytes().to_vec();
    out.extend_from_slice(name);
    Ok(Some(out))
}

impl Filesystem {
    /// Every extended attribute on an inode, in on-disk order.
    ///
    /// # Errors
    ///
    /// [`Error::BadSuperblock`] for an attribute fork whose structures do
    /// not fit where they claim to be, and whatever reading its blocks
    /// returns.
    pub fn list_xattrs(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<Xattr>> {
        let isize = usize::from(self.sb.inodesize);
        let Some((start, end)) = inode.attr_fork_range(isize) else {
            return Ok(Vec::new());
        };
        let fork = raw
            .get(start..end)
            .ok_or_else(|| corrupt(inode.ino, "fork past the inode record"))?;
        match inode.aformat {
            Format::Local => shortform(inode.ino, fork),
            Format::Extents | Format::Btree => {
                let extents = self.attr_extents(inode, fork)?;
                self.leaf_attrs(inode.ino, &extents)
            }
            other => Err(corrupt(
                inode.ino,
                &format!("a {other:?}-format attribute fork"),
            )),
        }
    }

    /// One attribute's value by its full name (`user.colour`), or `None`
    /// when it is not set. `Some(vec![])` is an attribute set to nothing.
    pub fn get_xattr(&self, inode: &Inode, raw: &[u8], name: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self
            .list_xattrs(inode, raw)?
            .into_iter()
            .find(|a| a.name == name)
            .map(|a| a.value))
    }

    fn attr_extents(&self, inode: &Inode, fork: &[u8]) -> Result<Vec<Extent>> {
        match inode.aformat {
            Format::Extents => extent::parse_list(fork, u64::from(inode.anextents)),
            _ => crate::bmbt::walk(
                fork,
                u64::from(inode.anextents),
                &self.sb,
                inode.ino,
                |fsblock| self.read_block_at(fsblock),
            ),
        }
    }

    fn read_block_at(&self, fsblock: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.sb.blocksize as usize];
        self.device.read_at(self.block_offset(fsblock), &mut buf)?;
        Ok(buf)
    }

    /// Attribute fork block `dablk` (fork-relative), through the fork's map.
    fn attr_block(&self, ino: u64, extents: &[Extent], dablk: u64) -> Result<Vec<u8>> {
        let e = extent::lookup(extents, dablk)
            .ok_or_else(|| corrupt(ino, &format!("block {dablk} is a hole")))?;
        let phys = e.map(dablk).expect("block inside its own extent");
        self.read_block_at(phys)
    }

    fn leaf_attrs(&self, ino: u64, extents: &[Extent]) -> Result<Vec<Xattr>> {
        let v5 = self.sb.is_v5();
        // Down the leftmost edge of a node B-tree to the first leaf.
        let mut at = 0u64;
        let mut block = self.attr_block(ino, extents, at)?;
        for _ in 0..=XFS_DA_NODE_MAXDEPTH {
            let magic = be16(&block, offsets::da_blkinfo::MAGIC);
            if magic != XFS_DA3_NODE_MAGIC && magic != XFS_DA_NODE_MAGIC {
                break;
            }
            let count_at = if v5 {
                offsets::node_hdr_v5::COUNT
            } else {
                offsets::node_hdr_v4::COUNT
            };
            if be16(&block, count_at) == 0 {
                return Err(corrupt(ino, "a node with no children"));
            }
            let first = node_hdr_size(v5) + offsets::node_entry::BEFORE;
            let before = block
                .get(first..first + 4)
                .map(|_| be32(&block, first))
                .ok_or_else(|| corrupt(ino, "node entry past the block"))?;
            at = u64::from(before);
            block = self.attr_block(ino, extents, at)?;
        }

        // Along the leaf chain. A chain cannot be longer than the fork has
        // blocks, so a loop in it is caught rather than followed forever.
        let fork_blocks: u64 = extents.iter().map(|e| e.blockcount).sum();
        let mut out = Vec::new();
        for _ in 0..=fork_blocks {
            let magic = be16(&block, offsets::da_blkinfo::MAGIC);
            if magic != XFS_ATTR3_LEAF_MAGIC && magic != XFS_ATTR_LEAF_MAGIC {
                return Err(corrupt(
                    ino,
                    &format!("block {at} has magic {magic:#06x}, not an attribute leaf"),
                ));
            }
            self.leaf_entries(ino, extents, &block, &mut out)?;
            let forw = be32(&block, offsets::da_blkinfo::FORW);
            if forw == 0 {
                return Ok(out);
            }
            at = u64::from(forw);
            block = self.attr_block(ino, extents, at)?;
        }
        Err(corrupt(ino, "the leaf chain loops"))
    }

    fn leaf_entries(
        &self,
        ino: u64,
        extents: &[Extent],
        leaf: &[u8],
        out: &mut Vec<Xattr>,
    ) -> Result<()> {
        let v5 = self.sb.is_v5();
        let count_at = if v5 {
            offsets::leaf_hdr_v5::COUNT
        } else {
            offsets::leaf_hdr_v4::COUNT
        };
        let count = usize::from(be16(leaf, count_at));
        let base = leaf_hdr_size(v5);
        if base + count * XFS_ATTR_LEAF_ENTRY_SIZE > leaf.len() {
            return Err(corrupt(ino, "leaf entries run past the block"));
        }
        for i in 0..count {
            let e = base + i * XFS_ATTR_LEAF_ENTRY_SIZE;
            let nameidx = usize::from(be16(leaf, e + offsets::leaf_entry::NAMEIDX));
            let entry_flags = leaf[e + offsets::leaf_entry::FLAGS];
            if entry_flags & flags::LOCAL != 0 {
                let hdr = offsets::leaf_name_local::NAMEVAL;
                let rec = leaf
                    .get(nameidx..nameidx + hdr)
                    .ok_or_else(|| corrupt(ino, "local name record past the block"))?;
                let valuelen = usize::from(be16(rec, offsets::leaf_name_local::VALUELEN));
                let namelen = usize::from(rec[offsets::leaf_name_local::NAMELEN]);
                let nameval = leaf
                    .get(nameidx + hdr..nameidx + hdr + namelen + valuelen)
                    .ok_or_else(|| corrupt(ino, "local name and value past the block"))?;
                if let Some(name) = full_name(ino, entry_flags, &nameval[..namelen])? {
                    out.push(Xattr {
                        name,
                        value: nameval[namelen..].to_vec(),
                    });
                }
            } else {
                let hdr = offsets::leaf_name_remote::NAME;
                let rec = leaf
                    .get(nameidx..nameidx + hdr)
                    .ok_or_else(|| corrupt(ino, "remote name record past the block"))?;
                let valueblk = be32(rec, offsets::leaf_name_remote::VALUEBLK);
                let valuelen = be32(rec, offsets::leaf_name_remote::VALUELEN);
                let namelen = usize::from(rec[offsets::leaf_name_remote::NAMELEN]);
                let name = leaf
                    .get(nameidx + hdr..nameidx + hdr + namelen)
                    .ok_or_else(|| corrupt(ino, "remote name past the block"))?;
                let Some(name) = full_name(ino, entry_flags, name)? else {
                    continue;
                };
                let value = self.remote_value(ino, extents, valueblk, valuelen)?;
                out.push(Xattr { name, value });
            }
        }
        Ok(())
    }

    /// A remote value: `valuelen` bytes over consecutive fork blocks from
    /// `valueblk`, each prefixed by a header on v5.
    fn remote_value(
        &self,
        ino: u64,
        extents: &[Extent],
        valueblk: u32,
        valuelen: u32,
    ) -> Result<Vec<u8>> {
        if valuelen > crate::format::attr::XFS_ATTR_VALUE_MAX {
            return Err(corrupt(ino, &format!("a {valuelen}-byte value")));
        }
        let v5 = self.sb.is_v5();
        let bs = self.sb.blocksize as usize;
        let header = if v5 { XFS_ATTR3_RMT_HDR_SIZE } else { 0 };
        let blocks = rmt_blocks(valuelen, bs, v5);
        let mut value = Vec::with_capacity(valuelen as usize);
        for n in 0..blocks as u64 {
            let block = self.attr_block(ino, extents, u64::from(valueblk) + n)?;
            let take = (valuelen as usize - value.len()).min(bs - header);
            value.extend_from_slice(&block[header..header + take]);
        }
        if value.len() != valuelen as usize {
            return Err(corrupt(ino, "a remote value shorter than it claims"));
        }
        Ok(value)
    }
}

/// `xfs_attr_shortform`: a four-byte header, then packed entries.
fn shortform(ino: u64, fork: &[u8]) -> Result<Vec<Xattr>> {
    if fork.len() < XFS_ATTR_SF_HDR_SIZE {
        return Err(corrupt(ino, "short-form header past the fork"));
    }
    let totsize = usize::from(be16(fork, offsets::sf_hdr::TOTSIZE));
    let count = usize::from(fork[offsets::sf_hdr::COUNT]);
    if totsize > fork.len() {
        return Err(corrupt(ino, "short-form attributes run past the fork"));
    }
    let mut out = Vec::with_capacity(count);
    let mut at = XFS_ATTR_SF_HDR_SIZE;
    for _ in 0..count {
        let hdr = fork
            .get(at..at + XFS_ATTR_SF_ENTRY_HDR_SIZE)
            .filter(|_| at + XFS_ATTR_SF_ENTRY_HDR_SIZE <= totsize)
            .ok_or_else(|| corrupt(ino, "short-form entry past totsize"))?;
        let namelen = usize::from(hdr[offsets::sf_entry::NAMELEN]);
        let valuelen = usize::from(hdr[offsets::sf_entry::VALUELEN]);
        let entry_flags = hdr[offsets::sf_entry::FLAGS];
        let nameval_at = at + offsets::sf_entry::NAMEVAL;
        let end = nameval_at + namelen + valuelen;
        if end > totsize {
            return Err(corrupt(ino, "short-form entry past totsize"));
        }
        // Everything short-form is local; LOCAL carries no meaning here.
        if let Some(name) = full_name(
            ino,
            entry_flags & !flags::LOCAL,
            &fork[nameval_at..nameval_at + namelen],
        )? {
            out.push(Xattr {
                name,
                value: fork[nameval_at + namelen..end].to_vec(),
            });
        }
        at = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Names come back with the prefix the disk leaves off, and what the
    /// format marks unfinished or does not define stays hidden.
    #[test]
    fn names_carry_their_namespace() {
        assert_eq!(full_name(1, 0, b"a").unwrap().unwrap(), b"user.a");
        assert_eq!(
            full_name(1, flags::ROOT, b"a").unwrap().unwrap(),
            b"trusted.a"
        );
        assert_eq!(
            full_name(1, flags::SECURE, b"a").unwrap().unwrap(),
            b"security.a"
        );
        assert_eq!(full_name(1, flags::INCOMPLETE, b"a").unwrap(), None);
        assert_eq!(full_name(1, 0x08, b"a").unwrap(), None, "an undefined bit");
        assert!(full_name(1, flags::ROOT | flags::SECURE, b"a").is_err());
    }

    /// A short-form fork: header, two entries, and a lie about the size.
    #[test]
    fn shortform_entries_and_bounds() {
        let mut fork = vec![0u8; 64];
        let mut at = 4;
        for (flags_byte, name, value) in
            [(0u8, &b"ab"[..], &b"xyz"[..]), (flags::SECURE, b"s", b"")]
        {
            fork[at] = name.len() as u8;
            fork[at + 1] = value.len() as u8;
            fork[at + 2] = flags_byte;
            fork[at + 3..at + 3 + name.len()].copy_from_slice(name);
            fork[at + 3 + name.len()..at + 3 + name.len() + value.len()].copy_from_slice(value);
            at += 3 + name.len() + value.len();
        }
        fork[0..2].copy_from_slice(&(at as u16).to_be_bytes());
        fork[2] = 2;
        let got = shortform(1, &fork).unwrap();
        assert_eq!(
            got,
            [
                Xattr {
                    name: b"user.ab".to_vec(),
                    value: b"xyz".to_vec()
                },
                Xattr {
                    name: b"security.s".to_vec(),
                    value: Vec::new()
                },
            ]
        );
        fork[0..2].copy_from_slice(&((at - 1) as u16).to_be_bytes());
        assert!(shortform(1, &fork).is_err(), "an entry past totsize");
    }
}
