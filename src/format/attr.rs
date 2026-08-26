//! Extended attributes: every on-disk shape, as constants.
//!
//! An XFS inode can carry name/value pairs alongside its data, and they
//! live in a second fork of the same inode — the *attribute fork*,
//! starting at `di_forkoff * 8` past the core and running to the end of
//! the inode record. [`crate::inode::Inode::attr_fork_range`] already
//! computes that span; `di_aformat` says how to read what is inside it.
//!
//! There is no attribute parser in this crate yet. This module is the
//! groundwork for one: every structure the format defines, named field
//! by field, so that the parser can be written against something already
//! checked against the specification and against real filesystems rather
//! than rediscovered a field at a time.
//!
//! # Which shape, and how to tell
//!
//! ```text
//! di_aformat = local   (1)  attributes are inline in the fork
//!                           -> xfs_attr_shortform
//! di_aformat = extents (2)  the fork is an extent list; the blocks it
//!                           maps hold either a single leaf block, or a
//!                           hash B-tree of node blocks above leaf
//!                           blocks — tell those two apart by the magic
//! di_aformat = btree   (3)  the fork holds a bmbt root; its leaves map
//!                           the same blocks the extents case does
//! ```
//!
//! Note what the format does *not* do: there is no "node" value of
//! `di_aformat`. Growing past a single leaf block changes nothing in the
//! inode — the fork is still `extents`, and the only evidence is that
//! attribute block 0 now carries [`XFS_DA3_NODE_MAGIC`] instead of
//! [`XFS_ATTR3_LEAF_MAGIC`]. A reader that switches on `di_aformat`
//! alone and assumes block 0 is a leaf will misparse every inode with
//! more attributes than fit in one block.
//!
//! # Traps
//!
//! **Block numbers in this fork are fork-relative.** `valueblk` in a
//! remote name record, `before` in a node record, and `forw`/`back` in
//! any block header are all `xfs_dablk_t` — offsets into the attribute
//! fork's own address space, not filesystem block numbers. Every one of
//! them has to go back through the fork's extent map before it names a
//! block on the device. They are the same width as an AG-relative block
//! number and will read as a plausible fsblock if used directly.
//!
//! **An attribute block is one filesystem block.** Directories may use a
//! larger logical block (`sb_dirblklog`); attributes never do. Verified
//! against a filesystem made with a 16 KiB directory block over a 4 KiB
//! filesystem block: its attribute leaf still occupied a single
//! filesystem block, with `firstused` inside 4096.
//!
//! **The short-form header is four bytes, not three.** The two fields
//! the specification lists — a 16-bit `totsize` and an 8-bit `count` —
//! come to three, but the structure is 16-bit aligned, so there is a
//! padding byte after `count` and the first entry starts at +4. See
//! [`XFS_ATTR_SF_HDR_SIZE`], where the arithmetic that settles it is
//! written out.
//!
//! **Local and remote entries are interleaved.** Whether an entry's name
//! record is an [`offsets::leaf_name_local`] or an
//! [`offsets::leaf_name_remote`] is decided per entry by [`flags::LOCAL`]
//! in the *index* entry, not by anything in the record itself, and the
//! two have different headers and different lengths. Reading a remote
//! record as a local one takes the name's length from the third byte of
//! `valueblk` and starts the name at the fourth — a short, wrong,
//! entirely printable name.
//!
//! **Remote values are not contiguous bytes.** On v5 every block of a
//! remote value begins with a 56-byte [`offsets::attr3_rmt_hdr`], so the
//! value has to be reassembled by stripping that header from each block.
//! On v4 there is no header and the blocks are raw. The block count
//! therefore differs between the two versions for the same value length;
//! see [`rmt_blocks`].
//!
//! # Byte order
//!
//! Big-endian, as everywhere else in XFS, with the standing exception
//! that CRCs are little-endian. Each structure below restates this for
//! itself, because the two `crc` fields in this module sit in the middle
//! of otherwise big-endian headers and are easy to read the wrong way
//! round.
//!
//! # Provenance
//!
//! Field layouts for the short-form, leaf and node structures, and the
//! v4 magic numbers, come from the published *XFS Filesystem Structure*
//! (SGI, 2nd edition revision 2). That document predates the v5
//! self-describing-metadata format entirely: it has no CRCs, no block
//! UUIDs and no `XARM` blocks. Everything v5 here — the `xfs_da3_blkinfo`
//! prefix, the v5 header sizes, [`XFS_ATTR3_LEAF_MAGIC`],
//! [`XFS_ATTR3_RMT_MAGIC`] and the whole of the remote-value header —
//! was established by building filesystems in the oracle VM and reading
//! the bytes back, as were the flag bit positions and the name hash. The
//! individual observations are recorded at the items they establish.

// Nothing in this module is called yet, and much of it will still not be
// called once there is a parser. That is the point: an offset can only
// be checked against the format documentation when its neighbours are
// named too, and this module exists so later work need not rediscover
// them.
#![allow(dead_code)]

// ---------------------------------------------------------------------
// Magic numbers
// ---------------------------------------------------------------------

/// `XFS_ATTR_LEAF_MAGIC` — a v4 attribute leaf block.
///
/// Sixteen bits, at [`offsets::da_blkinfo::MAGIC`], unlike the 32-bit
/// magic that fronts directory *data* blocks and B+tree blocks.
pub const XFS_ATTR_LEAF_MAGIC: u16 = 0xfbee;

/// `XFS_ATTR3_LEAF_MAGIC` — a v5 attribute leaf block.
///
/// The v5 magics are the v4 ones with the top nibble changed to 3, which
/// makes them easy to confuse when read in isolation: `0x3bee` against
/// `0xfbee` here, `0x3ebe` against `0xfebe` for nodes. Observed on a
/// `mkfs.xfs`-default (crc=1) filesystem.
pub const XFS_ATTR3_LEAF_MAGIC: u16 = 0x3bee;

/// `XFS_DA_NODE_MAGIC` — a v4 interior node of the attribute hash tree.
///
/// Shared with directories: the same structure indexes both, so this
/// value alone does not say which fork a block came from. The block's
/// `owner` does, on v5.
pub const XFS_DA_NODE_MAGIC: u16 = 0xfebe;

/// `XFS_DA3_NODE_MAGIC` — a v5 interior node of the attribute hash tree.
pub const XFS_DA3_NODE_MAGIC: u16 = 0x3ebe;

/// `XFS_ATTR3_RMT_MAGIC` — "XARM", one block of a remote attribute value
/// on v5.
///
/// A 32-bit magic, and the only attribute magic that is: remote value
/// blocks carry no `xfs_da_blkinfo`, so they are not part of the
/// forw/back-linked family the leaf and node blocks belong to. v4 has no
/// counterpart, because v4 remote value blocks have no header at all.
pub const XFS_ATTR3_RMT_MAGIC: u32 = 0x5841_524d;

// ---------------------------------------------------------------------
// Attribute fork format
// ---------------------------------------------------------------------

/// Values of `di_aformat`, the attribute fork's format.
///
/// The same encoding as `di_format` but restricted to three of its
/// values; [`crate::inode::Format`] decodes the full set. Named here as
/// raw bytes so a reader of this module can check them against the
/// `core.aformat` a disk examiner prints without leaving the file.
pub mod aformat {
    /// Attributes are stored inline in the fork, in short form.
    pub const LOCAL: u8 = 1;
    /// The fork is an array of extent records mapping attribute blocks.
    pub const EXTENTS: u8 = 2;
    /// The fork holds a bmbt root whose leaves map attribute blocks.
    pub const BTREE: u8 = 3;
}

// ---------------------------------------------------------------------
// Entry flags
// ---------------------------------------------------------------------

/// The `flags` byte, shared by short-form entries and leaf index
/// entries.
///
/// Bit positions were read off a real filesystem: each flag was set in
/// turn on a known leaf entry with a disk editor and the byte compared
/// against its former value. The entry started at `0x01`, and setting
/// `root`, `secure` and `incomplete` took it to `0x03`, `0x05` and
/// `0x81` respectively. The specification's own raw dumps agree for the
/// short-form case, where a `trusted.` attribute shows `0x02` and a
/// `security.` attribute `0x04`.
pub mod flags {
    /// The value is stored in the leaf block immediately after the name,
    /// and the entry's name record is an
    /// [`super::offsets::leaf_name_local`]. Clear means the value lives
    /// in separate blocks and the record is an
    /// [`super::offsets::leaf_name_remote`].
    ///
    /// Meaningless in a short-form entry — everything short-form is
    /// local by construction — so a short-form `flags` byte carries only
    /// the namespace bits.
    pub const LOCAL: u8 = 0x01;

    /// The attribute is in the root namespace, which Linux presents as
    /// `trusted.` and IRIX called `root`. Readable only by the
    /// superuser.
    pub const ROOT: u8 = 0x02;

    /// The attribute is in the `security.` namespace, used by security
    /// modules and by capability sets.
    pub const SECURE: u8 = 0x04;

    /// The attribute is half-created and must not be shown.
    ///
    /// A large value cannot be written in one transaction, so the entry
    /// is inserted with this bit set, the value blocks are filled, and
    /// the bit is cleared last. A filesystem interrupted in between has
    /// entries naming attributes whose values were never written; log
    /// recovery removes them. A reader that has satisfied itself the log
    /// is clean should treat a surviving incomplete entry as corruption,
    /// and one that has not replayed the log must at minimum not return
    /// it.
    pub const INCOMPLETE: u8 = 0x80;

    /// The namespace bits, as stored. Zero means the `user.` namespace,
    /// which has no bit of its own — so testing for `user` means testing
    /// that both of the others are clear, and a byte with *both* set is
    /// not a namespace this format defines.
    pub const NSP_ONDISK_MASK: u8 = ROOT | SECURE;

    /// The Linux namespace prefix an on-disk name should be reported
    /// under, or `None` for a flags byte claiming two namespaces at
    /// once.
    ///
    /// On-disk names are stored *without* the prefix, so a `user.foo`
    /// attribute is three bytes of name on disk. Anything presenting
    /// attributes through a POSIX interface has to put the prefix back,
    /// and anything hashing a name has to leave it off — see
    /// [`super::hashname`].
    pub const fn namespace_prefix(flags: u8) -> Option<&'static str> {
        match flags & NSP_ONDISK_MASK {
            0 => Some("user."),
            ROOT => Some("trusted."),
            SECURE => Some("security."),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------
// Limits
// ---------------------------------------------------------------------

/// Longest name the on-disk format can hold, in bytes.
///
/// Not a constant the format states: it is what a one-byte `namelen` can
/// count. The specification says names may be "up to 256 bytes …
/// terminated by the first 0 byte", describing the in-memory buffer
/// rather than the stored form; on disk the length is explicit and the
/// name is not terminated.
pub const XFS_ATTR_NAME_MAX: usize = u8::MAX as usize;

/// Longest value the on-disk format accepts, in bytes.
///
/// The specification says 64 KB. Confirmed as inclusive: a 65536-byte
/// value was accepted and stored, occupying 17 remote blocks on a 4 KiB
/// v5 filesystem.
pub const XFS_ATTR_VALUE_MAX: u32 = 65536;

/// Longest value a *short-form* attribute can hold, in bytes.
///
/// Short form counts its value length in one byte, so it is bounded far
/// below [`XFS_ATTR_VALUE_MAX`] regardless of how much room is left in
/// the inode. Anything longer forces the fork out to `extents`.
pub const XFS_ATTR_SF_VALUE_MAX: usize = u8::MAX as usize;

// ---------------------------------------------------------------------
// Field offsets
// ---------------------------------------------------------------------

/// Byte offsets within the on-disk attribute structures.
///
/// Named for the same reason the superblock's, the inode's and the
/// directory's are, and grouped the same way. Attributes bring their own
/// difficulty: three of the structures come in a v4 and a v5 shape that
/// differ only by the size of a header prefix, two of them put different
/// fields at the same offset, and one of them — the leaf's name record —
/// has two entirely different layouts chosen by a flag stored somewhere
/// else. Unnamed literals in that setting are unauditable.
pub mod offsets {
    /// `xfs_da_blkinfo` and its v5 extension `xfs_da3_blkinfo` — the
    /// prefix on every attribute leaf and node block.
    ///
    /// The same header directories use; [`crate::dir`] names it too, at
    /// the same offsets, and the two must not be allowed to drift apart.
    /// The first twelve bytes are identical in both versions, so `magic`
    /// can be read before the version is known — which is how a reader
    /// decides whether the twelve-byte or the fifty-six-byte form is in
    /// front of it.
    ///
    /// # Byte order
    ///
    /// Big-endian, except `crc`, which is little-endian.
    pub mod da_blkinfo {
        /// Next block at this level of the tree, as a fork-relative
        /// block number. Zero means none — block 0 is the root, so it
        /// can never be a sibling.
        pub const FORW: usize = 0;
        /// Previous block at this level, likewise.
        pub const BACK: usize = 4;
        /// Structure magic. Sixteen bits here, where a directory data
        /// block puts a 32-bit magic at offset 0.
        pub const MAGIC: usize = 8;
        /// Padding on v4; the low half of the v5 header's first word.
        /// Read in neither version.
        pub const PAD: usize = 10;
        /// CRC32C over the whole block with this field zeroed. Stored
        /// **little-endian**. v5 only.
        pub const CRC: usize = 12;
        /// The block's own disk address, in 512-byte units. Checking it
        /// catches a block read from the wrong place. v5 only.
        pub const BLKNO: usize = 16;
        /// Log sequence number of the last write to this block. v5 only.
        pub const LSN: usize = 24;
        /// The owning filesystem's UUID. v5 only.
        pub const UUID: usize = 32;
        /// Inode number of the inode this block belongs to — the one
        /// self-describing field that says *attribute fork of which
        /// file*, since the magic alone is shared with directories.
        /// v5 only.
        pub const OWNER: usize = 48;
    }

    /// `xfs_attr_sf_hdr` — the header of the short-form attribute list,
    /// at the very start of the attribute fork.
    ///
    /// # Byte order
    ///
    /// `totsize` is big-endian; the rest are single bytes.
    pub mod sf_hdr {
        /// Total bytes of short-form data, **including this header**.
        /// Adding it to the fork's start gives the end of the last
        /// entry.
        pub const TOTSIZE: usize = 0;
        /// Number of entries that follow.
        pub const COUNT: usize = 2;
        /// Padding, so that the entries begin on an even offset. Nothing
        /// reads it; it is named because [`super::super::XFS_ATTR_SF_HDR_SIZE`]
        /// being 4 rather than 3 depends on its existence.
        pub const PAD: usize = 3;
    }

    /// `xfs_attr_sf_entry` — one short-form attribute.
    ///
    /// Three bytes of header, then the name and the value back to back
    /// with nothing between them and no terminator. There is no index,
    /// no hash and no alignment: entries are walked from the first, and
    /// the only way to reach entry *n* is to have measured entries
    /// `0..n`.
    ///
    /// The alignment point is worth stating because it is the exception
    /// in this file. Everything else in the attribute format is aligned
    /// — 32-bit for leaf name records, 64-bit for the fork itself — and
    /// the specification says so explicitly, "except shortform
    /// attributes (they are tightly packed)".
    ///
    /// # Byte order
    ///
    /// Single bytes throughout; nothing here has an endianness.
    pub mod sf_entry {
        /// Length of the name, in bytes.
        pub const NAMELEN: usize = 0;
        /// Length of the value, in bytes. Zero for an attribute that
        /// exists but holds nothing, which is a normal thing for an
        /// attribute to be.
        pub const VALUELEN: usize = 1;
        /// Namespace bits — see [`super::super::flags`]. The local bit
        /// is meaningless here.
        pub const FLAGS: usize = 2;
        /// Name and value, concatenated: `namelen` bytes of name then
        /// `valuelen` bytes of value, no separator, no terminator.
        pub const NAMEVAL: usize = 3;
    }

    /// `xfs_attr_leaf_hdr` — the v4 leaf block header, following
    /// [`da_blkinfo`].
    ///
    /// The block is written from both ends:
    ///
    /// ```text
    /// 0                                                    blocksize
    /// +--------+----------------+···········+-----------------------+
    /// | header | entries[count] | free      | name/value records    |
    /// +--------+----------------+···········+-----------------------+
    ///          ^ packed, sorted              ^ firstused
    ///            by hash, 8 bytes              records grow downwards
    /// ```
    ///
    /// `entries` grows up from the header, sorted by `hashval`; the name
    /// and value records grow down from the end of the block. So
    /// `firstused - (hdr_size + count * ENTRY_SIZE)` is the gap between
    /// the two regions.
    ///
    /// # Traps
    ///
    /// **The entry array is the index; the records are not in its
    /// order.** `entries[i].nameidx` is a byte offset into the block,
    /// and consecutive entries point wherever their records happened to
    /// be allocated. Walking the record area and walking the entry array
    /// give the same set in different orders. The records are also not
    /// packed against each other once anything has been deleted — that
    /// is what `holes` reports.
    ///
    /// **`usedbytes` counts padded records**, not the sum of name and
    /// value lengths.
    ///
    /// **`firstused` is sixteen bits** and the largest XFS block is
    /// 65536 bytes, so field and block size meet exactly where the field
    /// can no longer represent a used region starting at offset 0. This
    /// module makes no claim about how that case is encoded; nothing
    /// here was tested above a 4 KiB block.
    ///
    /// # Byte order
    ///
    /// Big-endian, except the `crc` inherited from [`da_blkinfo`].
    pub mod leaf_hdr_v4 {
        /// Number of index entries.
        pub const COUNT: usize = 12;
        /// Bytes of the name/value region actually in use, counting the
        /// padding each record is rounded up by.
        pub const USEDBYTES: usize = 14;
        /// Byte offset of the lowest-addressed name/value record; the
        /// boundary between free space and used space.
        pub const FIRSTUSED: usize = 16;
        /// Non-zero when deleted records have left gaps a compaction
        /// would reclaim. Advisory: a reader never needs it, a writer
        /// uses it to decide whether to compact before splitting.
        pub const HOLES: usize = 18;
        /// Padding. Named so that `freemap` at 20 can be counted off.
        pub const PAD1: usize = 19;
        /// Start of the three-entry free-space map.
        pub const FREEMAP: usize = 20;
    }

    /// `xfs_attr3_leaf_hdr` — the v5 leaf block header, following
    /// [`da_blkinfo`].
    ///
    /// The same fields as [`leaf_hdr_v4`] in the same order, displaced by
    /// the larger block info, plus four bytes of tail padding. Read off
    /// a `mkfs.xfs`-default filesystem: a leaf holding twenty attributes
    /// carried, from offset 56,
    ///
    /// ```text
    /// 00 14 | 01 6c | 0e 94 | 00 | 00 | 00 f0 0d a4 | 0…0 | 00 00 00 00
    /// count   used    first   holes pad  freemap[0]    [1][2]  pad2
    ///  =20     =364    =3732                base 240
    ///                                       size 3492
    /// ```
    ///
    /// with the first index entry at 80. Everything said about
    /// [`leaf_hdr_v4`] applies here too.
    ///
    /// # Byte order
    ///
    /// Big-endian, except the `crc` inherited from [`da_blkinfo`].
    pub mod leaf_hdr_v5 {
        /// Number of index entries.
        pub const COUNT: usize = 56;
        /// Bytes of the name/value region in use, padding included.
        pub const USEDBYTES: usize = 58;
        /// Byte offset of the lowest-addressed name/value record.
        pub const FIRSTUSED: usize = 60;
        /// Whether compaction would reclaim anything.
        pub const HOLES: usize = 62;
        /// Padding.
        pub const PAD1: usize = 63;
        /// Start of the three-entry free-space map.
        pub const FREEMAP: usize = 64;
        /// Four bytes of tail padding — the whole of the difference
        /// between 76 and [`super::super::XFS_ATTR3_LEAF_HDR_SIZE`].
        /// Nothing reads it.
        pub const PAD2: usize = 76;
    }

    /// `xfs_attr_leaf_map` — one run of free bytes in the leaf header.
    ///
    /// # Byte order
    ///
    /// Both fields big-endian, in both versions.
    pub mod leaf_freemap {
        /// Byte offset of the run, measured from the **start of the
        /// block**, not from the end of the header. Verified: a
        /// twenty-entry v5 leaf recorded base 240, which is the 80-byte
        /// header plus the 160-byte entry array, not 160.
        pub const BASE: usize = 0;
        /// Length of the run, in bytes.
        pub const SIZE: usize = 2;
    }

    /// `xfs_attr_leaf_entry` — one index slot.
    ///
    /// Identical in both versions. The stride is 8 whether or not the
    /// last byte means anything, and the pad is part of it.
    ///
    /// # Byte order
    ///
    /// `hashval` and `nameidx` are big-endian; the rest are bytes.
    pub mod leaf_entry {
        /// Hash of the attribute's name, by [`super::super::hashname`].
        /// The array is sorted on this, ascending.
        pub const HASHVAL: usize = 0;
        /// Byte offset of the name record within this block. Not an
        /// index into anything, despite the name.
        pub const NAMEIDX: usize = 4;
        /// [`super::super::flags`]: namespace, local-or-remote, and the
        /// incomplete bit. The only place the local/remote decision is
        /// recorded.
        pub const FLAGS: usize = 6;
        /// Padding, to keep the entry eight bytes.
        pub const PAD2: usize = 7;
    }

    /// `xfs_attr_leaf_name_local` — the record for an entry with
    /// [`super::flags::LOCAL`] set: name and value both in this block.
    ///
    /// Identical in both versions.
    ///
    /// # Byte order
    ///
    /// `valuelen` is big-endian; `namelen` is one byte.
    pub mod leaf_name_local {
        /// Length of the value. Sixteen bits, so a local value may be
        /// far longer than a short-form one — up to whatever fits in the
        /// block.
        pub const VALUELEN: usize = 0;
        /// Length of the name.
        pub const NAMELEN: usize = 2;
        /// Name then value, concatenated, no separator and no
        /// terminator — the same packing short form uses.
        pub const NAMEVAL: usize = 3;
    }

    /// `xfs_attr_leaf_name_remote` — the record for an entry with
    /// [`super::flags::LOCAL`] clear: the name is here, the value is
    /// elsewhere.
    ///
    /// Identical in both versions, even though what the blocks it points
    /// at look like is not.
    ///
    /// # Byte order
    ///
    /// `valueblk` and `valuelen` are big-endian; `namelen` is one byte.
    pub mod leaf_name_remote {
        /// First block of the value, **as a fork-relative block
        /// number**. It must be mapped through the attribute fork's
        /// extent list; used as a filesystem block number it addresses
        /// something entirely unrelated near the start of the device.
        pub const VALUEBLK: usize = 0;
        /// Length of the value in bytes — the length of the *value*, not
        /// of the blocks holding it. On v5 those blocks carry per-block
        /// headers that this does not count.
        pub const VALUELEN: usize = 4;
        /// Length of the name.
        pub const NAMELEN: usize = 8;
        /// The name. Nothing follows it; the value is elsewhere.
        pub const NAME: usize = 9;
    }

    /// `xfs_da_node_hdr` — the v4 interior-node header, following
    /// [`da_blkinfo`].
    ///
    /// An interior node appears once the attributes outgrow a single
    /// leaf block. Attribute block 0 becomes the root and the leaves
    /// scatter; the inode is not touched, so `di_aformat` still says
    /// `extents`. The same structure indexes node-form directories, and
    /// [`crate::dir`] parses it there.
    ///
    /// # Lookup
    ///
    /// `btree` is sorted ascending on `hashval`, and each record's
    /// `hashval` is the **highest** hash in the subtree below it. So the
    /// child to descend into is the first record whose `hashval` is at
    /// or above the hash being looked up; a hash above every record in
    /// the root is simply not present.
    ///
    /// # Trap
    ///
    /// `count` and `level` occupy the two slots a *leaf* header uses for
    /// `count` and `usedbytes`. The two are told apart only by magic,
    /// and a node read as a leaf yields a plausible entry count followed
    /// by nonsense.
    ///
    /// # Byte order
    ///
    /// Big-endian, except the `crc` inherited from [`da_blkinfo`].
    pub mod node_hdr_v4 {
        /// Number of `btree` records.
        pub const COUNT: usize = 12;
        /// Height above the leaves. Leaves are level 0, so an interior
        /// node is 1 or more and the root's level is the tree's depth.
        pub const LEVEL: usize = 14;
    }

    /// `xfs_da3_node_hdr` — the v5 interior-node header, following
    /// [`da_blkinfo`].
    ///
    /// Read off a v5 root indexing 1000 attributes: at offset 56 the
    /// block held `00 0e | 00 01 | 00 00 00 00` — fourteen children, one
    /// level above the leaves, four bytes of pad — with the first record
    /// at 64. Everything said about [`node_hdr_v4`] applies here too.
    ///
    /// # Byte order
    ///
    /// Big-endian, except the `crc` inherited from [`da_blkinfo`].
    pub mod node_hdr_v5 {
        /// Number of `btree` records.
        pub const COUNT: usize = 56;
        /// Height above the leaves.
        pub const LEVEL: usize = 58;
        /// Padding to an eight-byte boundary — the whole of the
        /// difference between 60 and
        /// [`super::super::XFS_DA3_NODE_HDR_SIZE`]. Nothing reads it.
        pub const PAD32: usize = 60;
    }

    /// `xfs_da_node_entry` — one child record of an interior node.
    ///
    /// Identical in both versions.
    ///
    /// # Byte order
    ///
    /// Both fields big-endian.
    pub mod node_entry {
        /// Highest name hash in the subtree rooted at `before`.
        pub const HASHVAL: usize = 0;
        /// The child, as a **fork-relative** block number.
        pub const BEFORE: usize = 4;
    }

    /// `xfs_attr3_rmt_hdr` — the header on each block of a remote
    /// attribute value. **v5 only**; v4 remote blocks have no header.
    ///
    /// The leaf entry's [`leaf_name_remote`] record names the first block
    /// and the total length; the blocks run consecutively in the
    /// attribute fork's address space from there, so the *n*th block of
    /// the value is fork block `valueblk + n` — which still has to be
    /// mapped through the fork's extent list before it is a block on the
    /// device.
    ///
    /// # Why the version matters here more than anywhere else
    ///
    /// On v4 the value is `valuelen` bytes read straight off
    /// `ceil(valuelen / blocksize)` raw blocks. On v5 only
    /// `blocksize - 56` bytes of each block are value, so reassembling
    /// means skipping a header in *every* block, not just the first.
    /// Getting this backwards is not obvious in the output: the first
    /// 4040 bytes are right, and then 56 bytes of header appear in the
    /// middle of the data, once per block.
    ///
    /// # How the split was established
    ///
    /// A 28500-byte value on a 4 KiB v5 filesystem occupied nine blocks
    /// in total — one leaf and eight of value. `ceil(28500 / 4096)` is
    /// seven; `ceil(28500 / 4040)` is eight. The header was then read
    /// directly: the seventh block of a 30000-byte value carried
    /// `rm_offset = 0x5eb0` (24240, exactly `6 * 4040`) and the last
    /// carried `rm_bytes = 0x06b8` (1720), with `28280 + 1720 = 30000`
    /// matching the `valuelen` in the leaf.
    ///
    /// # Byte order
    ///
    /// Big-endian, except `crc`, which is little-endian.
    pub mod attr3_rmt_hdr {
        /// [`super::super::XFS_ATTR3_RMT_MAGIC`], "XARM". A 32-bit magic
        /// at offset 0, unlike the leaf and node blocks.
        pub const MAGIC: usize = 0;
        /// Byte offset of this block's data **within the value**, so
        /// `n * (blocksize - HDR_SIZE)` for the *n*th block. Lets a
        /// block be checked against the position it was reached from.
        pub const OFFSET: usize = 4;
        /// Bytes of value in this block: the full buffer space for every
        /// block but the last, and the remainder for that one.
        pub const BYTES: usize = 8;
        /// CRC32C over the block with this field zeroed, stored
        /// **little-endian**.
        pub const CRC: usize = 12;
        /// The owning filesystem's UUID.
        pub const UUID: usize = 16;
        /// Inode number of the file the value belongs to.
        pub const OWNER: usize = 32;
        /// The block's own disk address, in 512-byte units.
        pub const BLKNO: usize = 40;
        /// Log sequence number. Observed as all-ones on blocks written
        /// outside a logged transaction, which is the normal case for
        /// value data.
        pub const LSN: usize = 48;
    }
}

// ---------------------------------------------------------------------
// Structure sizes
// ---------------------------------------------------------------------

/// `sizeof(xfs_da_blkinfo)` — the v4 block prefix.
pub const XFS_DA_BLKINFO_SIZE: usize = 12;

/// `sizeof(xfs_da3_blkinfo)` — the v5 block prefix.
pub const XFS_DA3_BLKINFO_SIZE: usize = 56;

/// `sizeof(xfs_attr_sf_hdr)` — where the first short-form entry starts.
///
/// Four, not the three the two documented fields add up to. The
/// structure contains a `__be16` so it is two-byte aligned and the
/// compiler pads it; XFS sizes the on-disk header with `sizeof`, so the
/// padding is on disk too.
///
/// The arithmetic that settles it, from a filesystem carrying three
/// attributes — an empty `user.empty`, `trusted.trust` = `val1`, and
/// `security.pol` = `abc`:
///
/// ```text
/// entry sizes  3+5+0 =  8   ("empty", no value)
///              3+5+4 = 12   ("trust" + "val1")
///              3+3+3 =  9   ("pol"   + "abc")
///                     = 29
/// hdr.totsize          33   as stored
/// 33 - 29 = 4
/// ```
///
/// and the raw fork confirms it: `00 21 03 00 05 00 00 65 6d 70 74 79` —
/// `totsize` 0x21, `count` 3, one padding byte, then the first entry.
/// Sizing the header at 3 shifts every entry by one byte and yields a
/// name one character short with a stray leading byte.
pub const XFS_ATTR_SF_HDR_SIZE: usize = 4;

/// Bytes of `xfs_attr_sf_entry` before the name begins.
pub const XFS_ATTR_SF_ENTRY_HDR_SIZE: usize = 3;

/// `XFS_ATTR_LEAF_HDR_SIZE` for v4 — where `entries` begins.
///
/// Twelve bytes of block info, six of counts, two of flag and padding,
/// twelve of free map. Confirmed against a v4 filesystem with twenty
/// attributes: `freemap[0].base` read 192, and `32 + 20 * 8 = 192`.
pub const XFS_ATTR_LEAF_HDR_SIZE: usize = 32;

/// `XFS_ATTR3_LEAF_HDR_SIZE` for v5 — where `entries` begins.
///
/// Confirmed the same way as [`XFS_ATTR_LEAF_HDR_SIZE`]:
/// `freemap[0].base` was 240 on a twenty-entry leaf, and
/// `80 + 20 * 8 = 240`.
pub const XFS_ATTR3_LEAF_HDR_SIZE: usize = 80;

/// `XFS_ATTR_LEAF_MAPSIZE` — free runs recorded in a leaf header.
///
/// Only the three largest are kept; smaller runs are simply forgotten,
/// which is why the map understates the free space, and why an
/// allocation the map says will not fit may still fit after a
/// compaction.
pub const XFS_ATTR_LEAF_MAPSIZE: usize = 3;

/// `sizeof(xfs_attr_leaf_map)`.
pub const XFS_ATTR_LEAF_MAP_SIZE: usize = 4;

/// `sizeof(xfs_attr_leaf_entry)` — the index array's stride.
pub const XFS_ATTR_LEAF_ENTRY_SIZE: usize = 8;

/// Bytes of `xfs_attr_leaf_name_local` before the name.
pub const XFS_ATTR_LEAF_NAME_LOCAL_HDR_SIZE: usize = 3;

/// Bytes of `xfs_attr_leaf_name_remote` before the name.
pub const XFS_ATTR_LEAF_NAME_REMOTE_HDR_SIZE: usize = 9;

/// Leaf name records are aligned to this, in both versions.
///
/// The specification: "The name/value structures (both local and remote
/// versions) must be 32-bit aligned." Visible in a raw block as one to
/// three zero bytes between consecutive records.
pub const XFS_ATTR_LEAF_NAME_ALIGN: usize = 4;

/// `XFS_DA_NODE_HDR_SIZE` for v4 — where `btree` begins.
pub const XFS_DA_NODE_HDR_SIZE: usize = 16;

/// `XFS_DA3_NODE_HDR_SIZE` for v5 — where `btree` begins.
pub const XFS_DA3_NODE_HDR_SIZE: usize = 64;

/// `sizeof(xfs_da_node_entry)` — the `btree` array's stride.
pub const XFS_DA_NODE_ENTRY_SIZE: usize = 8;

/// `XFS_DA_NODE_MAXDEPTH` — deepest the hash tree is allowed to be.
///
/// Leaves are level 0, so an interior node's level is in `1..=4`, and a
/// claim above that is corruption rather than an unusually large
/// attribute set.
pub const XFS_DA_NODE_MAXDEPTH: u16 = 5;

/// `sizeof(xfs_attr3_rmt_hdr)` — bytes of each v5 remote block that are
/// not value. There is no v4 equivalent; on v4 the whole block is value.
pub const XFS_ATTR3_RMT_HDR_SIZE: usize = 56;

// ---------------------------------------------------------------------
// Derived sizes
// ---------------------------------------------------------------------

/// Where a leaf block's `entries` array begins, for whichever version is
/// in hand.
pub const fn leaf_hdr_size(is_v5: bool) -> usize {
    if is_v5 {
        XFS_ATTR3_LEAF_HDR_SIZE
    } else {
        XFS_ATTR_LEAF_HDR_SIZE
    }
}

/// Where an interior node's `btree` array begins, for whichever version
/// is in hand.
pub const fn node_hdr_size(is_v5: bool) -> usize {
    if is_v5 {
        XFS_DA3_NODE_HDR_SIZE
    } else {
        XFS_DA_NODE_HDR_SIZE
    }
}

/// `XFS_ATTR_SF_ENTSIZE` — total bytes one short-form entry occupies.
///
/// No rounding: the next entry begins here exactly.
pub const fn sf_entry_size(namelen: u8, valuelen: u8) -> usize {
    XFS_ATTR_SF_ENTRY_HDR_SIZE + namelen as usize + valuelen as usize
}

/// `XFS_ATTR_LEAF_ENTSIZE_LOCAL` — bytes a local name record occupies,
/// padding included.
///
/// Checked against a leaf whose header said `usedbytes = 364`: eleven
/// records of `3 + 7 + 8 = 18` rounding to 20, and nine of
/// `3 + 6 + 7 = 16` already aligned, give `11*20 + 9*16 = 364`.
pub const fn leaf_local_entsize(namelen: u8, valuelen: u16) -> usize {
    let n = XFS_ATTR_LEAF_NAME_LOCAL_HDR_SIZE + namelen as usize + valuelen as usize;
    (n + XFS_ATTR_LEAF_NAME_ALIGN - 1) & !(XFS_ATTR_LEAF_NAME_ALIGN - 1)
}

/// `XFS_ATTR_LEAF_ENTSIZE_REMOTE` — bytes a remote name record occupies,
/// padding included. The value contributes nothing; it is not here.
pub const fn leaf_remote_entsize(namelen: u8) -> usize {
    let n = XFS_ATTR_LEAF_NAME_REMOTE_HDR_SIZE + namelen as usize;
    (n + XFS_ATTR_LEAF_NAME_ALIGN - 1) & !(XFS_ATTR_LEAF_NAME_ALIGN - 1)
}

/// `XFS_ATTR3_RMT_BUF_SPACE` — value bytes one remote block can hold.
///
/// The whole block on v4; the block less its header on v5.
pub const fn rmt_buf_space(blocksize: usize, is_v5: bool) -> usize {
    if is_v5 {
        blocksize - XFS_ATTR3_RMT_HDR_SIZE
    } else {
        blocksize
    }
}

/// `xfs_attr3_rmt_blocks` — how many blocks a value of `valuelen` bytes
/// occupies.
///
/// Must divide by [`rmt_buf_space`], not by the block size, or a v5
/// value will be read a block short of its end for most lengths.
pub const fn rmt_blocks(valuelen: u32, blocksize: usize, is_v5: bool) -> usize {
    (valuelen as usize).div_ceil(rmt_buf_space(blocksize, is_v5))
}

// ---------------------------------------------------------------------
// The name hash
// ---------------------------------------------------------------------

/// A left rotation, named the way the hash's shape is usually described
/// so that the seven-bits-per-byte pattern below reads as one idea
/// rather than as three shifts and an or.
const fn rol32(x: u32, n: u32) -> u32 {
    x.rotate_left(n)
}

/// `xfs_da_hashname` — the hash stored in leaf and node entries.
///
/// The specification names this function but does not give it, so it was
/// reconstructed from the outside: a disk examiner will hash an
/// arbitrary string on request, and the shape is recoverable from short
/// inputs. One-, two- and three-byte names show the tail cases directly
/// (`"a"` hashes to `0x61`, `"ab"` to `0x30e2`, `"abc"` to `0x187163`),
/// four bytes show the body (`"abcd"` to `0x0c38b1e4`), and five and
/// nine bytes pin the rotation between rounds. It reproduces every one
/// of the twenty-three samples taken, including `"abcdefgh"` =
/// `0x4c7a38f6` and `"abcdefghi"` = `0x3d1c7b4f`, and it agrees with the
/// single hash the specification does print, `attribute_267` =
/// `0x3437d1a8`.
///
/// # What is hashed
///
/// The name **as stored** — without the namespace prefix. `user.foo` is
/// hashed as `foo`. Attributes in different namespaces with the same
/// stored name therefore share a hash, which is one more reason a lookup
/// must compare names and flags rather than stopping at the first hash
/// match. Duplicate hashes are possible even within one namespace, so
/// the entries a lookup must examine are a *range* of the sorted index,
/// not a single slot.
///
/// # Shape
///
/// Four bytes are consumed per round, each contributing seven bits of
/// shift, with the accumulator rotated seven bits per byte consumed. A
/// tail of one, two or three bytes is folded in the same way with a
/// correspondingly smaller rotation. The empty name hashes to zero.
pub const fn hashname(name: &[u8]) -> u32 {
    let mut hash: u32 = 0;
    let mut i = 0;
    let n = name.len();

    while n - i >= 4 {
        hash = ((name[i] as u32) << 21)
            ^ ((name[i + 1] as u32) << 14)
            ^ ((name[i + 2] as u32) << 7)
            ^ (name[i + 3] as u32)
            ^ rol32(hash, 7 * 4);
        i += 4;
    }

    match n - i {
        3 => {
            ((name[i] as u32) << 14)
                ^ ((name[i + 1] as u32) << 7)
                ^ (name[i + 2] as u32)
                ^ rol32(hash, 7 * 3)
        }
        2 => ((name[i] as u32) << 7) ^ (name[i + 1] as u32) ^ rol32(hash, 7 * 2),
        1 => (name[i] as u32) ^ rol32(hash, 7),
        // Zero, and unreachable for any other remainder: the loop above
        // leaves fewer than four bytes. A name whose length is a
        // multiple of four — including the empty name, which hashes to
        // zero — takes this arm with no folding left to do.
        _ => hash,
    }
}
