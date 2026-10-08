//! Journalled extended attribute replacement (#389).
//!
//! A replacement is built in fresh blocks. Remote payloads reach the device
//! before the transaction publishes their map; metadata, allocation changes,
//! and the inode are committed together. The old fork remains recoverable
//! until that commit. Layouts are documented in [`crate::format::attr`].

use crate::alloc_btree::FreeExtent;
use crate::attr::Xattr;
use crate::buf_write::BufferItem;
use crate::endian::{be16, be32, be64};
use crate::error::{Error, Result};
use crate::extent::{self, Extent};
use crate::format::attr::{self, flags, hashname};
use crate::format::log_items::buf_log_format::buf_type::{BLFT_ATTR_LEAF, BLFT_DA_NODE};
use crate::format::log_items::inode_log_format::offsets as ilf;
use crate::format::log_items::inode_log_format::{XFS_ILOG_ADATA, XFS_ILOG_AEXT, XFS_ILOG_DBROOT};
use crate::fs::Filesystem;
use crate::group_write::{split_fsblock, Allocations};
use crate::inode::{stamp_change, Changed, Format, Inode, XFS_DINODE_V3_SIZE};
use crate::log_write::{
    inode_log_format, log_dinode_from_disk, trans_header, InodeBuffer, Op, XFS_ILOG_CORE,
    XFS_TRANS_CHECKPOINT, XLOG_COMMIT_TRANS, XLOG_START_TRANS,
};

/// Whether an attribute may already exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XattrMode {
    /// Insert or replace.
    Set,
    /// Insert; fail with [`Error::AlreadyExists`] if present.
    Create,
    /// Replace; fail with [`Error::NotFound`] if absent.
    Replace,
}

fn unsupported(message: &str) -> Error {
    Error::UnsupportedFeature(message.into())
}

fn name_parts(name: &[u8]) -> Result<(u8, &[u8])> {
    let (namespace, short) = if let Some(short) = name.strip_prefix(b"user.") {
        (0, short)
    } else if let Some(short) = name.strip_prefix(b"trusted.") {
        (flags::ROOT, short)
    } else if let Some(short) = name.strip_prefix(b"security.") {
        (flags::SECURE, short)
    } else {
        return Err(unsupported(
            "supported attribute namespaces are user, trusted and security",
        ));
    };
    if short.is_empty() || name.len() > 255 || name.contains(&0) {
        return Err(unsupported(
            "attribute names must contain 1..255 non-NUL bytes including the namespace",
        ));
    }
    Ok((namespace, short))
}

fn align4(n: usize) -> usize {
    n.div_ceil(4) * 4
}

// A bmbt root's pointer array begins after its maximum key capacity,
// not its current record count. Resize both arrays when adding a fork.
fn resized_data_root(fs: &Filesystem, inode: &Inode, raw: &[u8]) -> Result<Vec<u8>> {
    fs.data_extents(inode, raw)?;
    let (start, end) = inode.data_fork_range(raw.len());
    let old = &raw[start..end];
    let count = usize::from(be16(old, 2));
    let size = (4 + count * 16).div_ceil(8) * 8;
    let mut root = vec![0; size];
    let old_pointers = 4 + (old.len() - 4) / 16 * 8;
    let new_pointers = 4 + (size - 4) / 16 * 8;
    root[..4 + count * 8].copy_from_slice(&old[..4 + count * 8]);
    root[new_pointers..new_pointers + count * 8]
        .copy_from_slice(&old[old_pointers..old_pointers + count * 8]);
    Ok(root)
}

struct Entry<'a> {
    namespace: u8,
    name: &'a [u8],
    value: &'a [u8],
    hash: u32,
    local: bool,
    record_size: usize,
    remote: u32,
}

enum Block {
    Leaf(Vec<usize>),
    Node { children: Vec<usize>, level: u16 },
}

/// Hash tree topology and remote ranges, independent of physical allocation.
struct Layout<'a> {
    entries: Vec<Entry<'a>>,
    blocks: Vec<Block>,
    total: u32,
}

impl<'a> Layout<'a> {
    fn new(attrs: &'a [Xattr], bs: usize) -> Result<Self> {
        let mut entries = Vec::with_capacity(attrs.len());
        for attr in attrs {
            let (namespace, name) = name_parts(&attr.name)?;
            let local_size = align4(3 + name.len() + attr.value.len());
            let local = local_size < bs / 2 - 16;
            entries.push(Entry {
                namespace,
                name,
                value: &attr.value,
                hash: hashname(name),
                local,
                record_size: if local {
                    local_size
                } else {
                    // The remote name starts at byte nine, but its record
                    // reserves the C format's twelve-byte minimum header.
                    align4(11 + name.len())
                },
                remote: 0,
            });
        }
        entries.sort_by_key(|e| (e.hash, e.namespace, e.name));
        let mut leaves: Vec<Block> = Vec::new();
        let mut indices = Vec::new();
        let mut used = 80;
        let mut at = 0;
        while at < entries.len() {
            let end = at + entries[at..].partition_point(|e| e.hash == entries[at].hash);
            let mut need: usize = entries[at..end].iter().map(|e| 8 + e.record_size).sum();
            // Keep a collision group in one leaf. Namespace prefixes are
            // not hashed, so three otherwise identical names can exhaust
            // a leaf with values individually below the local limit.
            if need > bs - 80 {
                for entry in &mut entries[at..end] {
                    if !entry.value.is_empty() {
                        entry.local = false;
                        entry.record_size = align4(11 + entry.name.len());
                    }
                }
                need = entries[at..end].iter().map(|e| 8 + e.record_size).sum();
                if need > bs - 80 {
                    return Err(unsupported(
                        "attribute hash collision group exceeds one leaf",
                    ));
                }
            }
            if used + need > bs {
                leaves.push(Block::Leaf(std::mem::take(&mut indices)));
                used = 80;
            }
            indices.extend(at..end);
            used += need;
            at = end;
        }
        leaves.push(Block::Leaf(indices));
        let mut blocks = if leaves.len() == 1 {
            leaves
        } else {
            let mut blocks = vec![Block::Node {
                children: Vec::new(),
                level: 0,
            }];
            blocks.extend(leaves);
            let capacity = (bs - 64) / 8;
            let mut children: Vec<_> = (1..blocks.len()).collect();
            let mut level = 1;
            while children.len() > capacity {
                let mut parents = Vec::new();
                for chunk in children.chunks(capacity) {
                    parents.push(blocks.len());
                    blocks.push(Block::Node {
                        children: chunk.to_vec(),
                        level,
                    });
                }
                children = parents;
                level += 1;
                if usize::from(level) > attr::XFS_DA_NODE_MAXDEPTH as usize {
                    return Err(unsupported(
                        "attribute hash tree exceeds the supported depth",
                    ));
                }
            }
            blocks[0] = Block::Node { children, level };
            blocks
        };
        // Keep this bound before allocating: one packed map record is enough.
        let mut total =
            u32::try_from(blocks.len()).map_err(|_| unsupported("attribute fork too large"))?;
        for entry in &mut entries {
            if !entry.local {
                entry.remote = total;
                total = total
                    .checked_add(attr::rmt_blocks(entry.value.len() as u32, bs, true) as u32)
                    .ok_or_else(|| unsupported("attribute fork too large"))?;
            }
        }
        Extent {
            startoff: 0,
            startblock: 0,
            blockcount: u64::from(total),
            unwritten: false,
        }
        .to_bytes()?;
        Ok(Self {
            entries,
            blocks: std::mem::take(&mut blocks),
            total,
        })
    }

    fn highest_hash(&self, block: usize) -> u32 {
        match &self.blocks[block] {
            Block::Leaf(entries) => self.entries[*entries.last().expect("nonempty leaf")].hash,
            Block::Node { children, .. } => {
                self.highest_hash(*children.last().expect("nonempty node"))
            }
        }
    }

    fn metadata(&self, fs: &Filesystem, ino: u64, start: u64) -> Vec<BufferItem> {
        let bs = fs.sb.blocksize as usize;
        let mut items = Vec::with_capacity(self.blocks.len());
        for (number, block) in self.blocks.iter().enumerate() {
            let mut bytes = vec![0; bs];
            let leaf = matches!(block, Block::Leaf(_));
            let magic = if leaf {
                attr::XFS_ATTR3_LEAF_MAGIC
            } else {
                attr::XFS_DA3_NODE_MAGIC
            };
            bytes[8..10].copy_from_slice(&magic.to_be_bytes());
            let disk_block = fs.block_offset(start + number as u64) / 512;
            bytes[16..24].copy_from_slice(&disk_block.to_be_bytes());
            bytes[32..48].copy_from_slice(&fs.sb.meta_uuid);
            bytes[48..56].copy_from_slice(&ino.to_be_bytes());
            // Blocks of the same level form a sibling chain. The root has none.
            let same_level = |candidate: &Block| match (block, candidate) {
                (Block::Leaf(_), Block::Leaf(_)) => true,
                (Block::Node { level: a, .. }, Block::Node { level: b, .. }) => a == b,
                _ => false,
            };
            let previous = (0..number)
                .rev()
                .find(|&i| same_level(&self.blocks[i]))
                .unwrap_or(0);
            let next = (number + 1..self.blocks.len())
                .find(|&i| same_level(&self.blocks[i]))
                .unwrap_or(0);
            bytes[0..4].copy_from_slice(&(next as u32).to_be_bytes());
            bytes[4..8].copy_from_slice(&(previous as u32).to_be_bytes());
            match block {
                Block::Leaf(indices) => {
                    bytes[56..58].copy_from_slice(&(indices.len() as u16).to_be_bytes());
                    let mut first = bs;
                    for (i, &entry) in indices.iter().enumerate() {
                        let entry = &self.entries[entry];
                        first -= entry.record_size;
                        let index = 80 + i * 8;
                        bytes[index..index + 4].copy_from_slice(&entry.hash.to_be_bytes());
                        bytes[index + 4..index + 6].copy_from_slice(&(first as u16).to_be_bytes());
                        bytes[index + 6] =
                            entry.namespace | if entry.local { flags::LOCAL } else { 0 };
                        let name_at = if entry.local {
                            bytes[first..first + 2]
                                .copy_from_slice(&(entry.value.len() as u16).to_be_bytes());
                            bytes[first + 2] = entry.name.len() as u8;
                            first + 3
                        } else {
                            bytes[first..first + 4].copy_from_slice(&entry.remote.to_be_bytes());
                            bytes[first + 4..first + 8]
                                .copy_from_slice(&(entry.value.len() as u32).to_be_bytes());
                            bytes[first + 8] = entry.name.len() as u8;
                            first + 9
                        };
                        bytes[name_at..name_at + entry.name.len()].copy_from_slice(entry.name);
                        if entry.local {
                            let value_at = name_at + entry.name.len();
                            bytes[value_at..value_at + entry.value.len()]
                                .copy_from_slice(entry.value);
                        }
                    }
                    bytes[58..60].copy_from_slice(&((bs - first) as u16).to_be_bytes());
                    // 64KiB uses zero to represent the end of the block.
                    bytes[60..62].copy_from_slice(&(first as u16).to_be_bytes());
                    let free_base = 80 + indices.len() * 8;
                    bytes[64..66].copy_from_slice(&(free_base as u16).to_be_bytes());
                    bytes[66..68].copy_from_slice(&((first - free_base) as u16).to_be_bytes());
                }
                Block::Node { children, level } => {
                    bytes[56..58].copy_from_slice(&(children.len() as u16).to_be_bytes());
                    bytes[58..60].copy_from_slice(&level.to_be_bytes());
                    for (i, &child) in children.iter().enumerate() {
                        let at = 64 + i * 8;
                        bytes[at..at + 4].copy_from_slice(&self.highest_hash(child).to_be_bytes());
                        bytes[at + 4..at + 8].copy_from_slice(&(child as u32).to_be_bytes());
                    }
                }
            }
            let mut item = BufferItem::new(
                disk_block,
                bytes,
                if leaf { BLFT_ATTR_LEAF } else { BLFT_DA_NODE },
                0,
            );
            item.mark(0, bs);
            items.push(item);
        }
        items
    }

    fn write_remote(&self, fs: &Filesystem, ino: u64, start: u64) -> Result<()> {
        let device = fs.writable.as_ref().ok_or(Error::ReadOnly)?;
        let bs = fs.sb.blocksize as usize;
        for entry in self.entries.iter().filter(|e| !e.local) {
            for (n, payload) in entry.value.chunks(bs - 56).enumerate() {
                let mut bytes = vec![0; bs];
                let address = fs.block_offset(start + u64::from(entry.remote) + n as u64);
                bytes[0..4].copy_from_slice(&attr::XFS_ATTR3_RMT_MAGIC.to_be_bytes());
                bytes[4..8].copy_from_slice(&((n * (bs - 56)) as u32).to_be_bytes());
                bytes[8..12].copy_from_slice(&(payload.len() as u32).to_be_bytes());
                bytes[16..32].copy_from_slice(&fs.sb.meta_uuid);
                bytes[32..40].copy_from_slice(&ino.to_be_bytes());
                bytes[40..48].copy_from_slice(&(address / 512).to_be_bytes());
                bytes[48..56].copy_from_slice(&u64::MAX.to_be_bytes());
                bytes[56..56 + payload.len()].copy_from_slice(payload);
                let crc = crate::superblock::crc32c_with_zeroed_crc(&bytes, 12);
                bytes[12..16].copy_from_slice(&crc.to_le_bytes());
                device.write_at(address, &bytes)?;
            }
        }
        device.flush()?;
        Ok(())
    }
}

fn shortform(attrs: &[Xattr], capacity: usize) -> Result<Option<Vec<u8>>> {
    if attrs.is_empty() {
        return Ok(Some(Vec::new()));
    }
    if attrs.len() > 255 {
        return Ok(None);
    }
    let mut bytes = vec![0; 4];
    bytes[2] = attrs.len() as u8;
    for attr in attrs {
        let (namespace, name) = name_parts(&attr.name)?;
        if attr.value.len() > 255 || bytes.len() + 3 + name.len() + attr.value.len() > capacity {
            return Ok(None);
        }
        bytes.extend_from_slice(&[name.len() as u8, attr.value.len() as u8, namespace]);
        bytes.extend_from_slice(name);
        bytes.extend_from_slice(&attr.value);
    }
    let len = bytes.len() as u16;
    bytes[0..2].copy_from_slice(&len.to_be_bytes());
    Ok(Some(bytes))
}

impl Filesystem {
    /// Set an attribute in the user, trusted or security namespace.
    ///
    /// Values may contain 0..=65536 bytes. Names include the namespace and
    /// contain at most 255 non-NUL bytes. Returns the transaction's LSN.
    /// ACL policy names are reserved; changing ACLs also requires updating
    /// the inode's mode and is not a generic attribute operation.
    ///
    /// # Errors
    /// Read-only mounts, v4 metadata, invalid names/values, and unsupported
    /// allocation layouts are refused before publishing an inode change.
    pub fn set_xattr(&self, ino: u64, name: &[u8], value: &[u8], mode: XattrMode) -> Result<u64> {
        self.change_xattr(ino, name, Some((value, mode)))
    }

    /// Remove a stored attribute, returning its transaction's LSN.
    /// Returns [`Error::NotFound`] when the attribute does not exist.
    pub fn remove_xattr(&self, ino: u64, name: &[u8]) -> Result<u64> {
        self.change_xattr(ino, name, None)
    }

    fn change_xattr(
        &self,
        ino: u64,
        name: &[u8],
        change: Option<(&[u8], XattrMode)>,
    ) -> Result<u64> {
        if self.writable.is_none() {
            return Err(Error::ReadOnly);
        }
        if !self.sb.is_v5() {
            return Err(unsupported("attribute writes require v5 metadata"));
        }
        name_parts(name)?;
        if matches!(name, b"trusted.SGI_ACL_FILE" | b"trusted.SGI_ACL_DEFAULT") {
            return Err(unsupported("ACL attributes require an ACL operation"));
        }
        if change.is_some_and(|(value, _)| value.len() > attr::XFS_ATTR_VALUE_MAX as usize) {
            return Err(unsupported("attribute values exceed 65536 bytes"));
        }
        let (inode, mut raw) = self.read_inode_raw(ino)?;
        let old_map = self.attribute_map(&inode, &raw)?;
        self.validate_attribute_fork(&inode, &raw, &old_map)?;
        let mut attrs = self.stored_xattrs(&inode, &raw)?;
        let existing = attrs.iter().position(|a| a.name == name);
        match (change, existing) {
            (Some((_, XattrMode::Create)), Some(_)) => return Err(Error::AlreadyExists),
            (Some((_, XattrMode::Replace)), None) | (None, None) => return Err(Error::NotFound),
            _ => {}
        }
        if let Some(index) = existing {
            attrs.remove(index);
        }
        if let Some((value, _)) = change {
            attrs.push(Xattr {
                name: name.to_vec(),
                value: value.to_vec(),
            });
        }
        let data_root = if inode.format == Format::Btree {
            Some(resized_data_root(self, &inode, &raw)?)
        } else {
            None
        };
        let data_required = match inode.format {
            Format::Local => {
                usize::try_from(inode.size).map_err(|_| unsupported("data fork too large"))?
            }
            Format::Extents => usize::try_from(inode.nextents)
                .map_err(|_| unsupported("data fork too large"))?
                .checked_mul(16)
                .ok_or_else(|| unsupported("data fork too large"))?,
            Format::Dev => 4,
            Format::Btree => data_root.as_ref().expect("data root").len(),
            _ => return Err(unsupported("unsupported inode data fork format")),
        }
        .max(8)
        .div_ceil(8)
            * 8;
        let start = XFS_DINODE_V3_SIZE
            .checked_add(data_required)
            .filter(|&start| start + 16 <= raw.len())
            .ok_or_else(|| unsupported("no space for an attribute map beside the data fork"))?;
        let inline = shortform(&attrs, raw.len() - start)?;
        let layout = if inline.is_none() {
            Some(Layout::new(&attrs, self.sb.blocksize as usize)?)
        } else {
            None
        };
        // Flush earlier overlays before any formerly freed blocks can become
        // remote payload blocks. A later overlay push must not overwrite them.
        self.sync()?;
        let mut allocations = Allocations::new();
        let mut metadata = Vec::new();
        let mut remote_start = None;
        let fork = if let Some(inline) = inline {
            inline
        } else {
            let layout = layout.as_ref().expect("external layout");
            let preferred = self.sb.split_ino(ino).0;
            let mut location = None;
            for attempt in 0..self.sb.agcount {
                let agno = (preferred + attempt) % self.sb.agcount;
                let group = allocations.group(&self.sb, self.device(), agno)?;
                match group.take(layout.total, ino as i64, crate::rmap::OFF_ATTR_FORK) {
                    Ok(block) => {
                        location = Some((u64::from(agno) << self.sb.agblklog) | u64::from(block));
                        break;
                    }
                    Err(Error::UnsupportedFeature(message))
                        if message.contains("has no single free run") =>
                    {
                        continue
                    }
                    Err(error) => return Err(error),
                }
            }
            let location = location.ok_or_else(|| {
                unsupported("no allocation group has a contiguous run for the attribute fork")
            })?;
            metadata = layout.metadata(self, ino, location);
            remote_start = Some(location);
            Extent {
                startoff: 0,
                startblock: location,
                blockcount: u64::from(layout.total),
                unwritten: false,
            }
            .to_bytes()?
            .to_vec()
        };
        let mut cancels = Vec::new();
        let old_blocks: u64 = old_map.iter().map(|e| e.blockcount).sum();
        for extent in old_map {
            let (agno, agblock) = split_fsblock(&self.sb, extent.startblock);
            let group = allocations.group(&self.sb, self.device(), agno)?;
            group.forget_rmap(crate::rmap::Rmap {
                startblock: agblock,
                blockcount: extent.blockcount as u32,
                owner: ino as i64,
                offset: extent.startoff | crate::rmap::OFF_ATTR_FORK,
            })?;
            group.give_back(FreeExtent {
                startblock: agblock,
                blockcount: extent.blockcount as u32,
            })?;
            // Cancel individual blocks: earlier metadata items name one block,
            // and the recovery cancellation table matches addresses and lengths.
            for n in 0..extent.blockcount {
                cancels.push(BufferItem::cancel(
                    self.block_offset(extent.startblock + n) / 512,
                    self.sb.blocksize / 512,
                ));
            }
        }
        let mut items = allocations.into_items()?;
        items.extend(metadata);
        raw[start..].fill(0);
        if let Some(root) = &data_root {
            raw[XFS_DINODE_V3_SIZE..start].copy_from_slice(root);
        }
        raw[start..start + fork.len()].copy_from_slice(&fork);
        raw[82] = (data_required / 8) as u8;
        let count = u32::from(layout.is_some());
        raw[83] = if attrs.is_empty() || layout.is_some() {
            Format::Extents as u8
        } else {
            Format::Local as u8
        };
        if self.sb.features_incompat & crate::superblock::incompat::NREXT64 != 0 {
            raw[76..80].copy_from_slice(&count.to_be_bytes());
        } else {
            raw[80..82].copy_from_slice(&(count as u16).to_be_bytes());
        }
        let nblocks = inode
            .nblocks
            .checked_sub(old_blocks)
            .and_then(|n| n.checked_add(layout.as_ref().map_or(0, |l| u64::from(l.total))))
            .ok_or_else(|| {
                Error::BadSuperblock("attribute blocks exceed inode block count".into())
            })?;
        raw[64..72].copy_from_slice(&nblocks.to_be_bytes());
        stamp_change(&mut raw, crate::create::clock_now(), Changed::Status);
        let changecount = be64(&raw, 104).wrapping_add(1);
        raw[104..112].copy_from_slice(&changecount.to_be_bytes());
        let core = log_dinode_from_disk(&raw).map_err(unsupported)?;
        let buffer =
            InodeBuffer::containing(self.inode_offset(ino)?, self.sb.inode_cluster_bytes());
        let fields = XFS_ILOG_CORE
            | if data_root.is_some() {
                XFS_ILOG_DBROOT
            } else {
                0
            }
            | if fork.is_empty() {
                0
            } else if layout.is_some() {
                XFS_ILOG_AEXT
            } else {
                XFS_ILOG_ADATA
            };
        let mut format = inode_log_format(ino, fields, &buffer);
        if !fork.is_empty() {
            format[ilf::ASIZE..ilf::ASIZE + 2].copy_from_slice(&(fork.len() as u16).to_le_bytes());
        }
        let logged_data_root = data_root
            .as_ref()
            .map(|root| crate::bmbt::inode_root_to_log(root, &self.sb, ino))
            .transpose()?;
        if let Some(root) = &logged_data_root {
            format[ilf::DSIZE..ilf::DSIZE + 2].copy_from_slice(&(root.len() as u16).to_le_bytes());
        }
        let inode_ops = 2 + usize::from(data_root.is_some()) + usize::from(!fork.is_empty());
        format[ilf::SIZE..ilf::SIZE + 2].copy_from_slice(&(inode_ops as u16).to_le_bytes());
        let item_ops = items
            .iter()
            .chain(&cancels)
            .map(BufferItem::op_count)
            .sum::<usize>()
            + inode_ops;
        // All fallible preparation precedes the ordered remote writes.
        if let Some(location) = remote_start {
            layout
                .as_ref()
                .expect("external layout")
                .write_remote(self, ino, location)?;
        }
        let lsn = self.commit_record(|tid| {
            let mut ops = vec![
                Op {
                    flags: XLOG_START_TRANS,
                    data: Vec::new(),
                },
                Op {
                    flags: 0,
                    data: trans_header(tid, XFS_TRANS_CHECKPOINT, item_ops as u32),
                },
            ];
            for item in items.iter().chain(&cancels) {
                ops.extend(item.ops());
            }
            ops.push(Op {
                flags: 0,
                data: format,
            });
            ops.push(Op {
                flags: 0,
                data: core,
            });
            if let Some(root) = logged_data_root {
                ops.push(Op {
                    flags: 0,
                    data: root,
                });
            }
            if !fork.is_empty() {
                let mut logged = fork;
                logged.resize(align4(logged.len()), 0);
                ops.push(Op {
                    flags: 0,
                    data: logged,
                });
            }
            ops.push(Op {
                flags: XLOG_COMMIT_TRANS,
                data: Vec::new(),
            });
            ops
        })?;
        self.logged_buffers(&items);
        self.logged_inode(ino, &raw, &[])?;
        Ok(lsn)
    }

    fn attribute_map(&self, inode: &Inode, raw: &[u8]) -> Result<Vec<Extent>> {
        let Some((start, end)) = inode.attr_fork_range(raw.len()) else {
            return Ok(Vec::new());
        };
        let fork = &raw[start..end];
        let (mut extents, mut blocks) = match inode.aformat {
            Format::Local => return Ok(Vec::new()),
            Format::Extents => (
                extent::parse_list(fork, u64::from(inode.anextents))?,
                Vec::new(),
            ),
            Format::Btree => crate::bmbt::walk_with_blocks(
                fork,
                u64::from(inode.anextents),
                &self.sb,
                inode.ino,
                |block| self.read_fsblock(block),
            )?,
            _ => return Err(unsupported("unsupported attribute fork format")),
        };
        self.check_extents_in_bounds(inode.ino, &extents)?;
        if extents.iter().any(|e| e.unwritten) {
            return Err(unsupported("unwritten attribute extent"));
        }
        blocks.sort_unstable();
        for block in blocks {
            if let Some(last) = extents.last_mut().filter(|e| {
                e.startoff == crate::rmap::OFF_BMBT_BLOCK
                    && e.startblock + e.blockcount == block
                    && split_fsblock(&self.sb, e.startblock).0 == split_fsblock(&self.sb, block).0
            }) {
                last.blockcount += 1;
            } else {
                extents.push(Extent {
                    startoff: crate::rmap::OFF_BMBT_BLOCK,
                    startblock: block,
                    blockcount: 1,
                    unwritten: false,
                });
            }
        }
        Ok(extents)
    }

    fn validate_attribute_fork(&self, inode: &Inode, raw: &[u8], extents: &[Extent]) -> Result<()> {
        let Some((start, end)) = inode.attr_fork_range(raw.len()) else {
            return Ok(());
        };
        if inode.aformat == Format::Local {
            let fork = &raw[start..end];
            let mut at = 4;
            for _ in 0..usize::from(
                *fork
                    .get(2)
                    .ok_or_else(|| unsupported("short attribute header"))?,
            ) {
                let header = fork
                    .get(at..at + 3)
                    .ok_or_else(|| unsupported("short attribute entry"))?;
                if header[2] & !(flags::ROOT | flags::SECURE) != 0 {
                    return Err(unsupported("unrecognized or incomplete stored attribute"));
                }
                at += 3 + usize::from(header[0]) + usize::from(header[1]);
            }
        }
        for extent in extents
            .iter()
            .filter(|e| e.startoff != crate::rmap::OFF_BMBT_BLOCK)
        {
            for n in 0..extent.blockcount {
                let physical = extent.startblock + n;
                let block = self.read_fsblock(physical)?;
                let remote = be32(&block, 0) == attr::XFS_ATTR3_RMT_MAGIC;
                let magic = be16(&block, 8);
                self.verify_attr_block(inode.ino, physical, &block)?;
                if !remote && magic == attr::XFS_ATTR3_LEAF_MAGIC {
                    let count = usize::from(be16(&block, 56));
                    if 80 + count * 8 > block.len() {
                        return Err(unsupported("attribute leaf entries exceed their block"));
                    }
                    for entry in block[80..80 + count * 8].chunks_exact(8) {
                        if entry[6] & !(flags::ROOT | flags::SECURE | flags::LOCAL) != 0 {
                            return Err(unsupported("unrecognized or incomplete stored attribute"));
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs_core::{BlockDevice, BlockRead};
    use std::collections::BTreeMap;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    };

    #[derive(Default)]
    struct SparseDevice {
        sectors: Mutex<BTreeMap<u64, [u8; 512]>>,
        fail_remote: AtomicBool,
    }

    impl BlockRead for SparseDevice {
        fn size_bytes(&self) -> u64 {
            crate::mkfs::MIN_DEVICE_BYTES
        }

        fn read_at(&self, mut offset: u64, mut bytes: &mut [u8]) -> fs_core::Result<()> {
            assert!(offset + bytes.len() as u64 <= self.size_bytes());
            let sectors = self.sectors.lock().unwrap();
            while !bytes.is_empty() {
                let within = offset as usize % 512;
                let count = bytes.len().min(512 - within);
                if let Some(sector) = sectors.get(&(offset / 512)) {
                    bytes[..count].copy_from_slice(&sector[within..within + count]);
                } else {
                    bytes[..count].fill(0);
                }
                offset += count as u64;
                bytes = &mut bytes[count..];
            }
            Ok(())
        }
    }

    impl BlockDevice for SparseDevice {
        fn is_writable(&self) -> bool {
            true
        }

        fn write_at(&self, mut offset: u64, mut bytes: &[u8]) -> fs_core::Result<()> {
            if bytes.starts_with(&attr::XFS_ATTR3_RMT_MAGIC.to_be_bytes())
                && self.fail_remote.load(Ordering::Relaxed)
            {
                return Err(fs_core::Error::Custom(
                    "injected remote write failure".into(),
                ));
            }
            assert!(offset + bytes.len() as u64 <= self.size_bytes());
            let mut sectors = self.sectors.lock().unwrap();
            while !bytes.is_empty() {
                let within = offset as usize % 512;
                let count = bytes.len().min(512 - within);
                let key = offset / 512;
                if bytes[..count].iter().any(|&b| b != 0) || sectors.contains_key(&key) {
                    sectors.entry(key).or_insert([0; 512])[within..within + count]
                        .copy_from_slice(&bytes[..count]);
                }
                offset += count as u64;
                bytes = &bytes[count..];
            }
            Ok(())
        }
    }

    fn formatted_device(block_size: u32) -> Arc<SparseDevice> {
        let device = Arc::new(SparseDevice::default());
        crate::mkfs::format(
            device.as_ref(),
            &crate::mkfs::Options {
                block_size,
                uuid: Some([0x38; 16]),
                ..Default::default()
            },
        )
        .unwrap();
        device
    }

    #[test]
    fn public_mutations_replay_across_fork_transitions() {
        let device = formatted_device(1024);
        let fs = Filesystem::mount_rw(device.clone()).unwrap();
        let ino = fs.superblock().rootino;
        fs.set_xattr(ino, b"user.keep", b"inline", XattrMode::Create)
            .unwrap();
        assert_eq!(
            fs.set_xattr(ino, b"user.keep", b"wrong", XattrMode::Create),
            Err(Error::AlreadyExists)
        );
        for i in 0..24 {
            fs.set_xattr(
                ino,
                format!("user.entry{i:02}").as_bytes(),
                &[i; 200],
                XattrMode::Set,
            )
            .unwrap();
        }
        fs.set_xattr(ino, b"user.keep", &[0x7b; 65536], XattrMode::Replace)
            .unwrap();
        let replay = Filesystem::mount(device.clone()).unwrap();
        let (inode, raw) = replay.read_inode_raw(ino).unwrap();
        assert_eq!(replay.list_xattrs(&inode, &raw).unwrap().len(), 25);
        assert_eq!(
            replay.get_xattr(&inode, &raw, b"user.keep").unwrap(),
            Some(vec![0x7b; 65536])
        );
        drop(replay);
        for i in 0..24 {
            fs.remove_xattr(ino, format!("user.entry{i:02}").as_bytes())
                .unwrap();
        }
        fs.set_xattr(ino, b"user.keep", b"small again", XattrMode::Replace)
            .unwrap();
        fs.remove_xattr(ino, b"user.keep").unwrap();
        assert_eq!(fs.remove_xattr(ino, b"user.keep"), Err(Error::NotFound));
        fs.sync().unwrap();
        drop(fs);
        let fs = Filesystem::mount(device).unwrap();
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        assert!(fs.list_xattrs(&inode, &raw).unwrap().is_empty());
        assert_eq!(inode.nblocks, 0);
    }

    #[test]
    fn failed_remote_write_preserves_the_committed_attribute() {
        let device = formatted_device(4096);
        let fs = Filesystem::mount_rw(device.clone()).unwrap();
        let ino = fs.superblock().rootino;
        fs.set_xattr(ino, b"user.keep", b"committed", XattrMode::Set)
            .unwrap();
        device.fail_remote.store(true, Ordering::Relaxed);
        assert!(fs
            .set_xattr(ino, b"user.keep", &[3; 65536], XattrMode::Replace)
            .is_err());
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        assert_eq!(
            fs.get_xattr(&inode, &raw, b"user.keep").unwrap(),
            Some(b"committed".to_vec())
        );
        drop(fs);
        device.fail_remote.store(false, Ordering::Relaxed);
        let fs = Filesystem::mount(device).unwrap();
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        assert_eq!(
            fs.get_xattr(&inode, &raw, b"user.keep").unwrap(),
            Some(b"committed".to_vec())
        );
    }

    fn attribute(name: &str, size: usize) -> Xattr {
        Xattr {
            name: name.as_bytes().to_vec(),
            value: vec![0xa5; size],
        }
    }

    #[test]
    fn names_require_a_known_namespace_and_a_nonempty_suffix() {
        for (name, namespace) in [
            ("user.a", 0),
            ("trusted.a", flags::ROOT),
            ("security.a", flags::SECURE),
        ] {
            assert_eq!(
                name_parts(name.as_bytes()).unwrap(),
                (namespace, b"a".as_slice())
            );
        }
        for name in [b"user.".as_slice(), b"user.a\0b", b"system.a", b"a"] {
            assert!(name_parts(name).is_err());
        }
        let mut name = b"user.".to_vec();
        name.resize(255, b'x');
        assert!(name_parts(&name).is_ok());
        name.push(b'x');
        assert!(name_parts(&name).is_err());
    }

    #[test]
    fn local_limit_is_exclusive_and_remote_values_strip_their_headers() {
        // With eight name bytes, 2017 rounds to 2028; 2018 rounds to
        // 2032, exactly the exclusive 4KiB leaf local limit.
        let attrs = vec![
            attribute("user.boundary", 2017),
            attribute("trusted.boundary", 2018),
            attribute("security.maximum", 65536),
        ];
        let layout = Layout::new(&attrs, 4096).unwrap();
        let local = layout.entries.iter().find(|e| e.namespace == 0).unwrap();
        let remote = layout
            .entries
            .iter()
            .find(|e| e.namespace == flags::ROOT)
            .unwrap();
        assert!(local.local);
        assert!(!remote.local);
        assert_eq!(attr::rmt_blocks(65536, 4096, true), 17);
        assert_eq!(layout.total as usize, layout.blocks.len() + 1 + 17);
    }

    #[test]
    fn shortform_checks_entry_width_capacity_and_count() {
        assert!(shortform(&[attribute("user.a", 255)], 263)
            .unwrap()
            .is_some());
        assert!(shortform(&[attribute("user.a", 255)], 262)
            .unwrap()
            .is_none());
        assert!(shortform(&[attribute("user.a", 256)], 4096)
            .unwrap()
            .is_none());
        let attrs: Vec<_> = (0..256)
            .map(|i| attribute(&format!("user.n{i}"), 0))
            .collect();
        assert!(shortform(&attrs, 65536).unwrap().is_none());
        assert_eq!(shortform(&[], 16).unwrap(), Some(Vec::new()));
    }

    #[test]
    fn hash_tree_adds_levels_without_losing_equal_hash_namespaces() {
        let mut attrs: Vec<_> = (0..2000)
            .map(|i| attribute(&format!("user.n{i:04}"), 64))
            .collect();
        attrs.extend([
            attribute("user.same", 3),
            attribute("trusted.same", 3),
            attribute("security.same", 3),
        ]);
        let layout = Layout::new(&attrs, 512).unwrap();
        assert!(matches!(layout.blocks[0], Block::Node { level: 2, .. }));
        let count: usize = layout
            .blocks
            .iter()
            .map(|b| match b {
                Block::Leaf(entries) => entries.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(count, attrs.len());
        let equal: Vec<_> = layout
            .entries
            .iter()
            .filter(|e| e.name == b"same")
            .collect();
        assert_eq!(equal.len(), 3);
        assert!(equal.iter().all(|e| e.hash == equal[0].hash));
        assert!(layout
            .entries
            .windows(2)
            .all(|pair| pair[0].hash <= pair[1].hash));
    }
}
