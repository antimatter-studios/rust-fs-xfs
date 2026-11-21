//! The evidence a salvage read returns: where, how sure, and what is wrong.

use std::fmt;

/// A byte range on the source device.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Extent {
    pub offset: u64,
    pub length: u64,
}

impl Extent {
    pub const fn new(offset: u64, length: u64) -> Self {
        Self { offset, length }
    }

    /// The first byte past the range, or `None` when it is not representable.
    pub fn end(&self) -> Option<u64> {
        self.offset.checked_add(self.length)
    }
}

/// The result of the format's own check on a structure's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Integrity {
    /// The structure's checksum matched its bytes.
    Verified,
    /// The checksum did not match: the structure is damaged evidence.
    Failed,
    /// The structure has no checksum (a v4 volume, or a v4 free list).
    Unchecked,
}

/// What kind of damage was seen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DamageKind {
    /// A magic number is wrong.
    Signature,
    /// A stored checksum disagrees with the bytes it covers.
    Checksum,
    /// Copies or structures that must agree do not: a superblock copy and
    /// the geometry, or a header and the volume or group it should belong to.
    Mismatch,
    /// A required structure is absent.
    Missing,
    /// A field holds a value the format does not allow.
    Malformed,
    /// A chain or map stops before the structure it describes is covered.
    Broken,
    /// The volume itself records a fault, such as the needs-repair flag or
    /// a log left needing replay.
    Recorded,
    /// The bytes could not be read.
    Unreadable,
}

/// How much the damage matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// State that may explain other damage, such as an unclean unmount.
    Notice,
    /// Damage a redundant copy covers.
    Warning,
    /// Damage that leaves a structure unusable as stored.
    Error,
}

/// One observation of damage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Damage {
    pub kind: DamageKind,
    pub severity: Severity,
    /// The damaged bytes, or for a missing structure where it should be.
    pub location: Extent,
    /// The structure holding the damage, in XFS's own terms: `superblock
    /// 0`, `AGF 3`.
    pub structure: String,
    /// What was observed, such as the stored and computed values.
    pub detail: String,
}

impl Damage {
    pub(crate) fn new(
        kind: DamageKind,
        severity: Severity,
        location: Extent,
        structure: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            severity,
            location,
            structure: structure.into(),
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Damage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:?} {:?} in {} at byte {} ({} bytes): {}",
            self.severity,
            self.kind,
            self.structure,
            self.location.offset,
            self.location.length,
            self.detail
        )
    }
}
