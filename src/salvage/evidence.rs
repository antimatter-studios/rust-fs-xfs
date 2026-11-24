//! The evidence a salvage read returns: where, how sure, and what is wrong.
//!
//! These are rust-fs-core's format-neutral salvage types, so this crate's
//! own salvage reads and its [`SalvageAdapter`](fs_core::layers::SalvageAdapter)
//! speak the same terms.

pub use fs_core::layers::{Damage, DamageKind, Extent, Integrity, Severity, WalkEnd};

/// One finding.
pub(crate) fn finding(
    kind: DamageKind,
    severity: Severity,
    location: Extent,
    structure: impl Into<String>,
    detail: impl Into<String>,
) -> Damage {
    Damage {
        kind,
        severity,
        location,
        structure: structure.into(),
        detail: detail.into(),
    }
}
