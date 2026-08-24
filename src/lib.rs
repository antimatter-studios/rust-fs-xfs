//! rust-fs-xfs — pure-Rust XFS filesystem driver.
//!
//! Exposes a stable C ABI (`fs_xfs_*`) so FFI consumers (Swift/C/Go/…)
//! can link `libfs_xfs.a` and `#include "fs_xfs.h"`.
//!
//! # Byte order
//!
//! XFS is **big-endian on disk** on every host. This is the one thing to
//! keep in mind when reading this crate alongside its little-endian
//! sisters (ext4, Btrfs, NTFS): every on-disk integer is decoded with
//! `from_be_bytes`.
//!
//! # Status
//!
//! Read path, v5 first. See the README for the supported-feature matrix.
//!
//! Architecture:
//! - [`error`] — driver error type, mapped to errno by the C ABI
//! - [`superblock`] — superblock parse + validation, geometry helpers
//! - [`ag`] — allocation group headers (AGF/AGI), with v5 identity checks

#![deny(unsafe_op_in_unsafe_fn)]

pub mod ag;
pub mod error;
pub mod inode;
pub mod superblock;

pub use error::{Error, Result};
pub use superblock::Superblock;
