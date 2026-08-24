//! Error type for the XFS driver.
//!
//! Mirrors the shape used by the sister `fs-*` crates so the C ABI layer
//! can map a driver error onto an errno the same way across drivers.

use std::fmt;

/// Everything that can go wrong reading or writing an XFS volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The underlying block device failed a read or write.
    Io(String),

    /// The superblock magic is not `XFSB` — this is not an XFS volume.
    NotXfs { magic: u32 },

    /// The volume is XFS, but a structural field is out of range or
    /// internally inconsistent. Carries a human-readable description of
    /// the specific field, because a bad geometry value is almost always
    /// the first symptom of reading the wrong offset.
    BadSuperblock(String),

    /// A metadata block failed its CRC32C check (v5 volumes only). The
    /// block header records where it believes it lives and who owns it,
    /// so a mismatch distinguishes corruption from a misdirected read.
    ChecksumMismatch {
        /// What kind of structure was being read.
        what: &'static str,
        /// Filesystem block address the block was read from.
        block: u64,
    },

    /// A metadata block's self-describing header disagrees with where it
    /// was read from, or which structure claimed it. On v5 this catches
    /// misdirected writes and stale block reuse that a CRC alone cannot.
    BlockIdentityMismatch {
        /// What kind of structure was being read.
        what: &'static str,
        /// Block address we read from.
        expected: u64,
        /// Block address the header claims.
        found: u64,
    },

    /// The volume uses an on-disk feature this driver does not implement.
    /// Distinct from [`Error::BadSuperblock`]: the volume is well-formed,
    /// we simply cannot honour it safely.
    UnsupportedFeature(String),

    /// The volume's log is dirty — it holds committed transactions that
    /// have not been applied to the metadata. Mounting without replaying
    /// it would present a stale, internally inconsistent filesystem.
    DirtyLog,

    /// A path component was not found.
    NotFound,

    /// A path component exists but is not a directory.
    NotADirectory,

    /// The operation requires a regular file.
    NotAFile,

    /// The requested operation would write, but the volume is mounted
    /// read-only or the driver has no write path for this structure.
    ReadOnly,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "device I/O failed: {m}"),
            Error::NotXfs { magic } => {
                write!(f, "not an XFS volume (superblock magic {magic:#010x})")
            }
            Error::BadSuperblock(m) => write!(f, "malformed XFS superblock: {m}"),
            Error::ChecksumMismatch { what, block } => {
                write!(f, "{what} at block {block} failed its CRC32C check")
            }
            Error::BlockIdentityMismatch {
                what,
                expected,
                found,
            } => write!(
                f,
                "{what} read from block {expected} claims to be block {found}"
            ),
            Error::UnsupportedFeature(m) => write!(f, "unsupported XFS feature: {m}"),
            Error::DirtyLog => f.write_str("XFS log is dirty and needs replay before mount"),
            Error::NotFound => f.write_str("no such file or directory"),
            Error::NotADirectory => f.write_str("not a directory"),
            Error::NotAFile => f.write_str("not a regular file"),
            Error::ReadOnly => f.write_str("filesystem is read-only"),
        }
    }
}

impl std::error::Error for Error {}

impl From<fs_core::Error> for Error {
    fn from(e: fs_core::Error) -> Self {
        Error::Io(e.to_string())
    }
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    //! The `Display` text here is not decoration. It is what reaches a
    //! user when a volume will not mount, and what a maintainer reads in
    //! a bug report, so each message is required to name the values that
    //! identify the problem. A message that says "checksum mismatch"
    //! without saying *where* costs an afternoon.

    use super::*;

    #[test]
    fn not_xfs_names_the_magic_it_found() {
        let e = Error::NotXfs { magic: 0x1234_5678 };
        let s = e.to_string();
        assert!(s.contains("0x12345678"), "magic missing from: {s}");
        assert!(s.contains("not an XFS volume"), "unclear message: {s}");
    }

    #[test]
    fn checksum_mismatch_names_the_structure_and_block() {
        let e = Error::ChecksumMismatch {
            what: "AGF",
            block: 42,
        };
        let s = e.to_string();
        assert!(s.contains("AGF"), "structure missing from: {s}");
        assert!(s.contains("42"), "block missing from: {s}");
    }

    #[test]
    fn identity_mismatch_names_both_addresses() {
        let e = Error::BlockIdentityMismatch {
            what: "inode",
            expected: 128,
            found: 129,
        };
        let s = e.to_string();
        assert!(s.contains("inode"), "structure missing from: {s}");
        assert!(
            s.contains("128") && s.contains("129"),
            "addresses missing: {s}"
        );
    }

    #[test]
    fn bad_superblock_carries_its_detail() {
        let e = Error::BadSuperblock("blocksize 7 is not a power of two".into());
        assert!(e.to_string().contains("blocksize 7"));
    }

    #[test]
    fn unsupported_feature_carries_its_detail() {
        let e = Error::UnsupportedFeature("bmbt walker is not implemented".into());
        assert!(e.to_string().contains("bmbt walker"));
    }

    #[test]
    fn io_carries_the_underlying_message() {
        let e = Error::Io("device went away".into());
        assert!(e.to_string().contains("device went away"));
    }

    /// The unit-like variants still have to say something useful.
    #[test]
    fn unit_variants_render_meaningfully() {
        for (e, expect) in [
            (Error::DirtyLog, "replay"),
            (Error::NotFound, "no such file"),
            (Error::NotADirectory, "not a directory"),
            (Error::NotAFile, "not a regular file"),
            (Error::ReadOnly, "read-only"),
        ] {
            let s = e.to_string();
            assert!(
                s.contains(expect),
                "{e:?} rendered as {s:?}, which does not mention {expect:?}"
            );
        }
    }

    /// Every variant must render as non-empty text, so a future variant
    /// added without a match arm cannot slip through as a blank message.
    #[test]
    fn every_variant_renders_non_empty() {
        let all = [
            Error::Io("x".into()),
            Error::NotXfs { magic: 0 },
            Error::BadSuperblock("x".into()),
            Error::ChecksumMismatch {
                what: "x",
                block: 0,
            },
            Error::BlockIdentityMismatch {
                what: "x",
                expected: 0,
                found: 1,
            },
            Error::UnsupportedFeature("x".into()),
            Error::DirtyLog,
            Error::NotFound,
            Error::NotADirectory,
            Error::NotAFile,
            Error::ReadOnly,
        ];
        for e in all {
            assert!(!e.to_string().is_empty(), "{e:?} rendered as empty text");
        }
    }

    #[test]
    fn wraps_a_device_error() {
        let inner = fs_core::Error::ReadOnly;
        let e: Error = inner.into();
        assert!(matches!(e, Error::Io(_)), "device errors become Io");
        assert!(!e.to_string().is_empty());
    }

    #[test]
    fn implements_std_error() {
        fn assert_is_error<T: std::error::Error>() {}
        assert_is_error::<Error>();
    }
}
