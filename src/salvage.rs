//! Reading a damaged volume: the structures one at a time, without a mount.
//!
//! [`Filesystem::mount`](crate::Filesystem::mount) refuses a volume whose
//! primary superblock fails its checksum, and every strict parser in this
//! crate stops at the first structure that does not check out. That is
//! right for a mount, and useless for getting data back off a volume that
//! is damaged. This module reads the same structures the other way:
//!
//! - **Nothing is trusted, nothing is dropped.** A structure that fails its
//!   checksum, names another volume or holds an impossible field is still
//!   decoded where its bytes allow, and returned with the failure beside
//!   it as [`Damage`].
//! - **Read-only by type.** Every function takes the source as
//!   `&dyn BlockRead`, which has no write path. A dirty log is reported,
//!   never replayed onto the source.
//! - **Plain data.** Results hold no handle to the source, so they can be
//!   kept, compared and serialised by a caller.
//!
//! The evidence types ([`Extent`], [`Integrity`], [`Damage`]) have the
//! shape of the format-neutral salvage types the rust-fs drivers share, so
//! that this crate's own functions map onto them directly.
//!
//! What is here today:
//!
//! - [`superblocks`]: the primary superblock and every copy, the geometry
//!   chosen from them, and where they disagree.
//! - [`scan_superblocks`]: superblocks found by their magic in a byte
//!   range, for a volume whose primary is too damaged to say where the
//!   copies are.
//! - [`ag_headers`]: the AGF, AGI and AGFL of one allocation group.
//! - [`log_health`]: whether the log was left needing replay.
//! - [`damage`]: all of the above for the whole volume, as one list.
//! - [`find_inodes`]: inode records found by their magic in a byte range,
//!   live, free and damaged, and [`read_inode`] for one by number.
//! - [`walk_extents`]: the storage an inode's data fork claims, kept as far
//!   as its map can be followed, and [`read_mapped`] to read through it.

mod evidence;
mod headers;
mod inodes;

pub use evidence::{Damage, DamageKind, Extent, Integrity, Severity};
pub use headers::{
    ag_headers, damage, log_health, read_superblock, scan_superblocks, superblocks, AgHeaders,
    Header, SuperblockCopy, Superblocks, VolumeDamage,
};
pub use inodes::{
    find_inodes, inode_location, inode_number_at, read_inode, read_mapped, walk_extents, ExtentMap,
    FileExtent, InodeCandidate, InodeScan, InodeState, WalkEnd,
};
