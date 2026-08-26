//! On-disk structures, named but not yet used.
//!
//! Every structure either filesystem defines, written down as offsets,
//! sizes, magic numbers and sentinels, whether or not anything in this
//! crate reads it today. It is a reservoir: work that needs a layout
//! should find it here rather than rediscover it.
//!
//! # Why hold structures nothing calls
//!
//! Because an offset is only checkable against the format documentation
//! when its neighbours are named too. A parser that reads three fields
//! out of a twenty-field structure leaves the next reader counting bytes
//! to decide whether `56` is right, and counting bytes is how a
//! transposed constant survives review. The same reasoning already
//! governs the `offsets` modules beside the parsers; this is that habit
//! applied to the whole format rather than to the parts in use.
//!
//! It is also where the expensive knowledge goes. Several layouts here
//! cost hours to establish and cannot be looked up, for the reason
//! below.
//!
//! # Where these came from, and why it is not all one source
//!
//! The published specification — *XFS Filesystem Structure*, SGI, 2006,
//! and marked incomplete on its own title page — documents **v4 only**.
//! It contains no checksums, no self-describing block headers, and none
//! of the v5 magic numbers. Every filesystem in use today is v5.
//!
//! So the provenance is mixed, and each module says which of these
//! applies to each structure:
//!
//! - **the published specification**, for v4 layouts and the field
//!   offsets the v5 structures inherit
//! - **measurement against filesystems the kernel wrote**, for
//!   everything v5 adds, and for the journal, whose chapter in the
//!   specification is the word `TODO:` and nothing else
//! - **this crate's own parsers**, which are the authority where they
//!   already name something
//!
//! Where a value was measured rather than read, the module says so. A
//! reader deciding whether to trust a constant is better served by
//! knowing it was seen in four hundred records than by being told it is
//! correct.
//!
//! # v4 is here, but v5 is the target
//!
//! Both versions are recorded, and reading a v4 volume stays supported.
//! Writing is a different matter: the write paths target **v5 only**.
//!
//! That is not a shortcut. v5 has been what `mkfs.xfs` produces since
//! 2014 and is the only version receiving format work, so a v4 write
//! path would be effort spent on volumes that are no longer being
//! created. It also removes a class of conditional from the write side
//! outright — in v5 every metadata block carries a fixed-size
//! self-describing header, so structure sizes stop depending on which
//! superblock features are set, and there is one layout to get right
//! instead of two that must agree.
//!
//! The v4 constants stay because reading needs them and because they are
//! what the specification actually documents: they are the evidence for
//! the v5 offsets that were derived by measuring the difference.
//!
//! # These are constants, not parsers
//!
//! Nothing here reads a device or allocates. The modules are
//! deliberately free of dependencies on the rest of the crate so that a
//! layout can be checked in isolation, and so that a mistake here cannot
//! break anything that compiles today.

pub mod attr;
pub mod dir;
pub mod log_items;
