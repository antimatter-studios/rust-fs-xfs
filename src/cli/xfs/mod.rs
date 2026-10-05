//! The XFS tools: what this repository fills the shared contract in
//! with. Everything filesystem-specific lives here, and nothing here is
//! plumbing (that is `fs_core::cli`, in am-fs-core).

pub mod device;
pub mod fs;
pub mod mkfs;
