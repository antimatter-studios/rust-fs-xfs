//! Writing POSIX ACLs (#390).
//!
//! XFS keeps an inode's access ACL in `trusted.SGI_ACL_FILE` and a
//! directory's default ACL in `trusted.SGI_ACL_DEFAULT`, as `struct
//! xfs_acl`: a big-endian entry count, then twelve-byte entries of tag, id
//! and permission bits. [`crate::format::acl`] names both that format and
//! the one the VFS hands across as `system.posix_acl_*`.
//!
//! # An access ACL and the mode are one thing
//!
//! The owner, owning-group (or mask) and other entries of an access ACL
//! *are* the permission bits of the mode, so the two are written together
//! in one record: setting an access ACL sets the mode it implies, and
//! [`Filesystem::chmod`] sets the matching entries of the ACL, as the
//! kernel's `posix_acl_chmod` does. An access ACL with nothing beyond those
//! three entries says no more than the mode, and is stored as the mode
//! alone, as `posix_acl_equiv_mode` decides.
//!
//! # Inheritance
//!
//! A directory's default ACL is what an inode made in it starts with
//! (`posix_acl_create`): a new inode's access ACL is the default with its
//! owner, group-class and other entries narrowed by the mode asked for, and
//! a new directory carries the default on as its own. The inode is made
//! with the narrowed mode first and the ACL written after it, so a crash
//! between the two leaves an inode no more open than intended, never more.

use crate::attr_write::XattrMode;
use crate::error::{Error, Result};
use crate::format::acl::{tag, xfs_acl_max_entries, XFS_ACL_ENTRY_SIZE, XFS_ACL_HDR_SIZE};
use crate::fs::Filesystem;

/// The stored name of an access ACL.
const ACCESS: &[u8] = b"trusted.SGI_ACL_FILE";
/// The stored name of a default ACL.
const DEFAULT: &[u8] = b"trusted.SGI_ACL_DEFAULT";

/// What a new inode inherits: its narrowed permission bits, the access
/// ACL to store when it says more than they do, and a directory's default.
pub(crate) type Inherited = (u16, Option<Vec<u8>>, Option<Vec<u8>>);

/// Which of an inode's two ACLs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AclKind {
    /// The access ACL: who may do what to this inode.
    Access,
    /// The default ACL: what an inode made in this directory starts with.
    Default,
}

impl AclKind {
    fn name(self) -> &'static [u8] {
        match self {
            AclKind::Access => ACCESS,
            AclKind::Default => DEFAULT,
        }
    }
}

/// One ACL entry: a tag from [`crate::format::acl::tag`], the user or
/// group it names (for `USER` and `GROUP` only), and `rwx` bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AclEntry {
    pub tag: u32,
    pub id: u32,
    pub perm: u16,
}

/// The entries sorted and checked as `posix_acl_valid` checks them:
/// exactly one owner, owning-group and other entry, a mask whenever a named
/// user or group is present, no name twice, and permissions within `rwx`.
fn valid(entries: &[AclEntry], v5: bool) -> Result<Vec<AclEntry>> {
    let bad = |why: String| Error::UnsupportedFeature(format!("not a valid POSIX ACL: {why}"));
    let mut sorted: Vec<AclEntry> = entries
        .iter()
        .map(|e| AclEntry {
            id: if matches!(e.tag, tag::USER | tag::GROUP) {
                e.id
            } else {
                0
            },
            ..*e
        })
        .collect();
    sorted.sort_by_key(|e| (e.tag, e.id));
    let count = |t: u32| sorted.iter().filter(|e| e.tag == t).count();
    for (t, what) in [
        (tag::USER_OBJ, "owner"),
        (tag::GROUP_OBJ, "owning group"),
        (tag::OTHER, "other"),
    ] {
        if count(t) != 1 {
            return Err(bad(format!("{} {what} entries, not one", count(t))));
        }
    }
    if count(tag::MASK) > 1 {
        return Err(bad("more than one mask".into()));
    }
    if (count(tag::USER) + count(tag::GROUP)) > 0 && count(tag::MASK) == 0 {
        return Err(bad("named entries without a mask".into()));
    }
    for e in &sorted {
        if !matches!(
            e.tag,
            tag::USER_OBJ | tag::USER | tag::GROUP_OBJ | tag::GROUP | tag::MASK | tag::OTHER
        ) {
            return Err(bad(format!("the tag {:#x}", e.tag)));
        }
        if e.perm > 7 {
            return Err(bad(format!("permissions {:#o}", e.perm)));
        }
    }
    if sorted
        .windows(2)
        .any(|w| w[0].tag == w[1].tag && w[0].id == w[1].id)
    {
        return Err(bad("a user or group named twice".into()));
    }
    if sorted.len() > xfs_acl_max_entries(v5) {
        return Err(bad(format!("{} entries", sorted.len())));
    }
    Ok(sorted)
}

/// `struct xfs_acl`, big-endian.
fn encode(entries: &[AclEntry]) -> Vec<u8> {
    let mut out = Vec::with_capacity(XFS_ACL_HDR_SIZE + entries.len() * XFS_ACL_ENTRY_SIZE);
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in entries {
        out.extend_from_slice(&e.tag.to_be_bytes());
        out.extend_from_slice(&e.id.to_be_bytes());
        out.extend_from_slice(&e.perm.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
    }
    out
}

/// The entries of a stored `struct xfs_acl`.
fn decode(stored: &[u8]) -> Result<Vec<AclEntry>> {
    if stored.len() < XFS_ACL_HDR_SIZE {
        return Err(Error::UnsupportedFeature(
            "a stored ACL shorter than its header".into(),
        ));
    }
    let count = u32::from_be_bytes(stored[0..4].try_into().expect("4")) as usize;
    if stored.len() != XFS_ACL_HDR_SIZE + count * XFS_ACL_ENTRY_SIZE {
        return Err(Error::UnsupportedFeature(
            "a stored ACL whose length is not its count's".into(),
        ));
    }
    Ok(stored[XFS_ACL_HDR_SIZE..]
        .chunks_exact(XFS_ACL_ENTRY_SIZE)
        .map(|e| AclEntry {
            tag: u32::from_be_bytes(e[0..4].try_into().expect("4")),
            id: u32::from_be_bytes(e[4..8].try_into().expect("4")),
            perm: u16::from_be_bytes(e[8..10].try_into().expect("2")),
        })
        .collect())
}

/// The group-class entry: the mask when there is one, the owning group
/// otherwise. It is what the mode's group bits stand for.
fn group_class(entries: &mut [AclEntry]) -> &mut AclEntry {
    let at = entries
        .iter()
        .position(|e| e.tag == tag::MASK)
        .or_else(|| entries.iter().position(|e| e.tag == tag::GROUP_OBJ))
        .expect("a valid ACL has an owning group");
    &mut entries[at]
}

/// The permission bits an access ACL implies.
fn mode_of(entries: &[AclEntry]) -> u16 {
    let perm = |t: u32| entries.iter().find(|e| e.tag == t).map_or(0, |e| e.perm);
    let group = if entries.iter().any(|e| e.tag == tag::MASK) {
        perm(tag::MASK)
    } else {
        perm(tag::GROUP_OBJ)
    };
    (perm(tag::USER_OBJ) << 6) | (group << 3) | perm(tag::OTHER)
}

/// An ACL's owner, group-class and other entries narrowed to `mode`, as
/// `posix_acl_create` narrows a default for a new inode.
fn narrowed(mut entries: Vec<AclEntry>, mode: u16) -> Vec<AclEntry> {
    for e in entries.iter_mut() {
        match e.tag {
            tag::USER_OBJ => e.perm &= (mode >> 6) & 7,
            tag::OTHER => e.perm &= mode & 7,
            _ => {}
        }
    }
    group_class(&mut entries).perm &= (mode >> 3) & 7;
    entries
}

impl Filesystem {
    /// The ACL of `kind` on inode `ino`, or `None` when it has none.
    ///
    /// # Errors
    ///
    /// Whatever reading the inode or its attributes returns.
    pub fn acl(&self, ino: u64, kind: AclKind) -> Result<Option<Vec<AclEntry>>> {
        let (inode, raw) = self.read_inode_raw(ino)?;
        self.list_xattrs(&inode, &raw)?
            .into_iter()
            .find(|a| a.name == kind.name())
            .map(|a| decode(&a.value))
            .transpose()
    }

    /// Set the ACL of `kind` on inode `ino` to `entries` (#390).
    ///
    /// An access ACL sets the permission bits it implies in the same
    /// record, and one with only owner, owning-group and other entries is
    /// stored as those bits alone. A default ACL is a directory's only.
    /// Returns the sequence number of the record.
    ///
    /// # Errors
    ///
    /// [`Error::UnsupportedFeature`] for entries that are not a valid POSIX
    /// ACL, [`Error::NotADirectory`] for a default ACL on anything else,
    /// and whatever writing the attribute returns.
    pub fn set_acl(&self, ino: u64, kind: AclKind, entries: &[AclEntry]) -> Result<u64> {
        let entries = valid(entries, self.sb.is_v5())?;
        let (inode, _) = self.read_inode_raw(ino)?;
        match kind {
            AclKind::Default => {
                if !inode.is_dir() {
                    return Err(Error::NotADirectory);
                }
                self.write_xattr(
                    ino,
                    DEFAULT,
                    Some((&encode(&entries), XattrMode::Set)),
                    None,
                )
            }
            AclKind::Access => {
                let bits = (inode.mode & !0o777 & 0o7777) | mode_of(&entries);
                let stored = self.acl(ino, AclKind::Access)?.is_some();
                if entries.len() == 3 {
                    // Equivalent to the mode: the bits alone, and no ACL.
                    if stored {
                        self.write_xattr(ino, ACCESS, None, Some(bits))
                    } else {
                        self.set_permissions(ino, bits)
                    }
                } else {
                    self.write_xattr(
                        ino,
                        ACCESS,
                        Some((&encode(&entries), XattrMode::Set)),
                        Some(bits),
                    )
                }
            }
        }
    }

    /// Remove the ACL of `kind` from inode `ino`. The mode is left as it is.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when there is none, and whatever writing the
    /// attribute returns.
    pub fn remove_acl(&self, ino: u64, kind: AclKind) -> Result<u64> {
        self.write_xattr(ino, kind.name(), None, None)
    }

    /// Set inode `ino`'s permission bits, keeping an access ACL in step:
    /// its owner, group-class and other entries take the new bits, in the
    /// same record, as the kernel's `posix_acl_chmod` does (#390).
    ///
    /// # Errors
    ///
    /// Whatever reading the inode or writing it returns.
    pub fn chmod(&self, ino: u64, permissions: u16) -> Result<u64> {
        let bits = permissions & 0o7777;
        match self.acl(ino, AclKind::Access)? {
            Some(mut entries) => {
                for e in entries.iter_mut() {
                    match e.tag {
                        tag::USER_OBJ => e.perm = (bits >> 6) & 7,
                        tag::OTHER => e.perm = bits & 7,
                        _ => {}
                    }
                }
                group_class(&mut entries).perm = (bits >> 3) & 7;
                self.write_xattr(
                    ino,
                    ACCESS,
                    Some((&encode(&entries), XattrMode::Replace)),
                    Some(bits),
                )
            }
            None => self.set_permissions(ino, bits),
        }
    }

    /// Set only the permission bits, through the log.
    fn set_permissions(&self, ino: u64, bits: u16) -> Result<u64> {
        let (_, raw) = self.read_inode_raw(ino)?;
        let mut core = raw.clone();
        let mode = (u16::from_be_bytes([core[2], core[3]]) & !0o7777) | (bits & 0o7777);
        core[2..4].copy_from_slice(&mode.to_be_bytes());
        let now = u64::from_be_bytes(core[104..112].try_into().expect("8"));
        core[104..112].copy_from_slice(&now.wrapping_add(1).to_be_bytes());
        crate::inode::stamp_change(
            &mut core,
            crate::create::clock_now(),
            crate::inode::Changed::Status,
        );
        self.log_inode_core(ino, &core)
    }

    /// What an inode made in `parent` with `mode` inherits: the mode
    /// narrowed by the parent's default ACL, the access ACL to write when it
    /// says more than that mode, and, for a directory, the default to carry
    /// on. `None` when the parent has no default ACL.
    pub(crate) fn inherited_acl(
        &self,
        parent: u64,
        mode: u16,
        is_dir: bool,
    ) -> Result<Option<Inherited>> {
        let Some(default) = self.acl(parent, AclKind::Default)? else {
            return Ok(None);
        };
        let access = narrowed(default.clone(), mode);
        let bits = (mode & !0o777 & 0o7777) | mode_of(&access);
        let access_bytes = (access.len() > 3).then(|| encode(&access));
        let default_bytes = is_dir.then(|| encode(&default));
        Ok(Some((bits, access_bytes, default_bytes)))
    }

    /// Give a newly made inode the ACLs it inherited.
    pub(crate) fn write_inherited_acl(
        &self,
        ino: u64,
        access: Option<Vec<u8>>,
        default: Option<Vec<u8>>,
    ) -> Result<()> {
        if let Some(bytes) = access {
            self.write_xattr(ino, ACCESS, Some((&bytes, XattrMode::Create)), None)?;
        }
        if let Some(bytes) = default {
            self.write_xattr(ino, DEFAULT, Some((&bytes, XattrMode::Create)), None)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(tag: u32, id: u32, perm: u16) -> AclEntry {
        AclEntry { tag, id, perm }
    }

    #[test]
    fn a_valid_acl_is_sorted_and_encoded_as_xfs_stores_it() {
        let acl = valid(
            &[
                e(tag::OTHER, 0, 4),
                e(tag::USER, 1000, 6),
                e(tag::MASK, 0, 6),
                e(tag::USER_OBJ, 0, 7),
                e(tag::GROUP_OBJ, 0, 5),
            ],
            true,
        )
        .unwrap();
        assert_eq!(acl[0].tag, tag::USER_OBJ);
        let bytes = encode(&acl);
        assert_eq!(&bytes[0..4], &5u32.to_be_bytes());
        assert_eq!(decode(&bytes).unwrap(), acl);
        assert_eq!(
            mode_of(&acl),
            0o764,
            "the mask, not the owning group, is the group bits"
        );
    }

    #[test]
    fn what_posix_acl_valid_refuses_is_refused() {
        assert!(valid(&[e(tag::USER_OBJ, 0, 7), e(tag::OTHER, 0, 4)], true).is_err());
        assert!(valid(
            &[
                e(tag::USER_OBJ, 0, 7),
                e(tag::GROUP_OBJ, 0, 5),
                e(tag::USER, 7, 6),
                e(tag::OTHER, 0, 4)
            ],
            true
        )
        .is_err());
        assert!(valid(
            &[
                e(tag::USER_OBJ, 0, 8),
                e(tag::GROUP_OBJ, 0, 5),
                e(tag::OTHER, 0, 4)
            ],
            true
        )
        .is_err());
    }

    #[test]
    fn a_default_is_narrowed_by_the_mode_asked_for() {
        let default = vec![
            e(tag::USER_OBJ, 0, 7),
            e(tag::USER, 1000, 7),
            e(tag::GROUP_OBJ, 0, 7),
            e(tag::MASK, 0, 7),
            e(tag::OTHER, 0, 7),
        ];
        let access = narrowed(default, 0o640);
        assert_eq!(mode_of(&access), 0o640);
        let user = access.iter().find(|x| x.tag == tag::USER).unwrap();
        assert_eq!(
            user.perm, 7,
            "a named entry is kept and masked, not narrowed"
        );
    }
}
