//! POSIX ACLs: how XFS stores one, and how the VFS is shown it (#285).
//!
//! An ACL is an attribute in the root namespace, [`SGI_ACL_FILE`] for a
//! file's access ACL and [`SGI_ACL_DEFAULT`] for a directory's default,
//! and its value is `struct xfs_acl` (`fs/xfs/libxfs/xfs_format.h`): a
//! big-endian entry count, then [`XFS_ACL_ENTRY_SIZE`]-byte entries of
//! tag, id and permissions, all big-endian.
//!
//! The kernel does not show those bytes as the ACL. `fs/xfs/xfs_acl.c`
//! decodes them into a `posix_acl`, and the VFS hands that out as
//! [`POSIX_ACL_ACCESS`] / [`POSIX_ACL_DEFAULT`] in its own format,
//! `posix_acl_xattr_header` (`include/uapi/linux/posix_acl_xattr.h`): a
//! little-endian version word, [`POSIX_ACL_XATTR_VERSION`], then
//! [`POSIX_ACL_XATTR_ENTRY_SIZE`]-byte entries of tag and permissions
//! (16 bits each) and id (32 bits), all little-endian. The kernel's own
//! listing under root, `getfattr -d -m - -e hex`, is what
//! `tests/posix_acl_view_oracle.rs` checks both against.

/// The root-namespace attribute holding a file's access ACL.
pub const SGI_ACL_FILE: &[u8] = b"SGI_ACL_FILE";

/// The root-namespace attribute holding a directory's default ACL.
pub const SGI_ACL_DEFAULT: &[u8] = b"SGI_ACL_DEFAULT";

/// The VFS name a file's access ACL is shown under.
pub const POSIX_ACL_ACCESS: &[u8] = b"system.posix_acl_access";

/// The VFS name a directory's default ACL is shown under.
pub const POSIX_ACL_DEFAULT: &[u8] = b"system.posix_acl_default";

/// `sizeof(struct xfs_acl)` before its entries: `acl_cnt`, `__be32`.
pub const XFS_ACL_HDR_SIZE: usize = 4;

/// `sizeof(struct xfs_acl_entry)`: `ae_tag` and `ae_id`, `__be32` each,
/// `ae_perm`, `__be16`, and two bytes the compiler pads it to.
pub const XFS_ACL_ENTRY_SIZE: usize = 12;

/// Byte offsets within one `struct xfs_acl_entry`.
pub mod entry {
    /// `ae_tag`, `__be32`: one of the [`super::tag`] values.
    pub const TAG: usize = 0;
    /// `ae_id`, `__be32`: the uid or gid of a named entry.
    pub const ID: usize = 4;
    /// `ae_perm`, `__be16`: `r` 4, `w` 2, `x` 1.
    pub const PERM: usize = 8;
}

/// `XFS_ACL_MAX_ENTRIES`: how many entries a stored ACL may have.
///
/// A v5 filesystem allows as many as fit in the longest attribute value,
/// `(XFS_XATTR_SIZE_MAX - sizeof(struct xfs_acl)) / sizeof(struct
/// xfs_acl_entry)`. A v4 one keeps IRIX's limit of 25. The kernel refuses
/// to read an ACL with more.
pub const fn xfs_acl_max_entries(is_v5: bool) -> usize {
    if is_v5 {
        (super::attr::XFS_ATTR_VALUE_MAX as usize - XFS_ACL_HDR_SIZE) / XFS_ACL_ENTRY_SIZE
    } else {
        25
    }
}

/// `e_tag` values (`include/linux/posix_acl.h`), which the stored and the
/// VFS format share.
pub mod tag {
    /// The owner: `user::`.
    pub const USER_OBJ: u32 = 0x01;
    /// A named user: `user:<uid>:`.
    pub const USER: u32 = 0x02;
    /// The owning group: `group::`.
    pub const GROUP_OBJ: u32 = 0x04;
    /// A named group: `group:<gid>:`.
    pub const GROUP: u32 = 0x08;
    /// The mask: `mask::`.
    pub const MASK: u32 = 0x10;
    /// Everyone else: `other::`.
    pub const OTHER: u32 = 0x20;
}

/// `POSIX_ACL_XATTR_VERSION`: the VFS format's `a_version`.
pub const POSIX_ACL_XATTR_VERSION: u32 = 0x0002;

/// `sizeof(struct posix_acl_xattr_entry)`: `e_tag` and `e_perm`,
/// `__le16` each, then `e_id`, `__le32`.
pub const POSIX_ACL_XATTR_ENTRY_SIZE: usize = 8;

/// `ACL_UNDEFINED_ID`: the `e_id` the VFS gives an entry that names
/// nobody (every tag but [`tag::USER`] and [`tag::GROUP`]).
pub const ACL_UNDEFINED_ID: u32 = u32::MAX;
