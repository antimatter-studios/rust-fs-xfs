//! Overwriting file data in place.
//!
//! This is the only kind of write that XFS lets a driver make without a
//! journal, and the reason is worth stating precisely, because every
//! other write does need one.
//!
//! XFS journals **metadata**, not data. File contents go straight to
//! their blocks; the log holds the changes to inodes, extents,
//! allocation trees and directories. So an overwrite that touches no
//! metadata touches nothing the log is responsible for — and an
//! overwrite of bytes that already exist, inside an extent that is
//! already allocated and already written, is exactly that. The inode's
//! size does not change. Its extent list does not change. No block is
//! allocated or freed.
//!
//! What that buys is a write path that cannot corrupt a filesystem. If
//! the machine dies halfway through, the file holds a mixture of old and
//! new bytes — which is precisely what happens when the kernel's own
//! write is interrupted, and precisely what every application that
//! cares about already defends against. The filesystem itself remains
//! consistent, because nothing describing it was touched.
//!
//! # What this refuses, and why each one needs the journal
//!
//! Every refusal below is a case where completing the write would
//! require a metadata change, and a metadata change without a log entry
//! is how a filesystem gets corrupted rather than merely a file.
//!
//! | Refused | What it would take |
//! |---|---|
//! | Writing past the end of the file | growing `di_size` — an inode write |
//! | Writing into a hole | allocating blocks — free-space trees and the extent list |
//! | Writing into an unwritten extent | clearing the unwritten flag — an extent-list write |
//! | Writing to a reflinked inode | breaking the share — copy-on-write and refcount trees |
//! | Writing an inline (`Local`) file | the data *is* inode bytes, so writing it is a metadata write |
//! | Writing to a real-time inode | a device this driver does not address |
//!
//! Refusing is not a placeholder for these. It is the correct answer
//! until the log writer exists, and approximating any of them would
//! produce a filesystem that its own checker rejects.

use crate::error::{Error, Result};
use crate::extent;
use crate::inode::{offsets as inode_offsets, Format, Inode, Timestamp};
use crate::superblock::crc32c_with_zeroed_crc;
use crate::Filesystem;

impl Filesystem {
    /// Overwrite `data` into an existing file at `offset`.
    ///
    /// Returns the number of bytes written, which is always `data.len()`
    /// on success — a short write is not possible here, because every
    /// condition that would shorten it is checked before anything is
    /// written. Nothing is written at all unless the whole range can be.
    ///
    /// `raw` is the inode's on-disk bytes, as returned alongside the
    /// inode by the read path.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless the filesystem was opened with
    /// [`Filesystem::mount_rw`]. [`Error::NotAFile`] for anything that
    /// is not a regular file. [`Error::UnsupportedFeature`] for each of
    /// the cases in the table above, naming which one it was — a caller
    /// deciding whether to fall back needs to know whether it hit a
    /// hole or a shared extent, not merely that the write was declined.
    pub fn write_at(&self, inode: &Inode, raw: &[u8], offset: u64, data: &[u8]) -> Result<usize> {
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if data.is_empty() {
            return Ok(0);
        }

        // A symlink is excluded as well as the obvious non-files: a short
        // one lives inline in the inode, and a long one's target length
        // is inode state, so neither can be rewritten without a metadata
        // write.
        if !inode.is_regular_file() {
            return Err(Error::NotAFile);
        }
        if inode.is_realtime() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} keeps its data on the real-time device",
                inode.ino
            )));
        }
        if inode.has_shared_extents() {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} has reflinked extents; writing one in place would change \
                 what another inode reads",
                inode.ino
            )));
        }
        if inode.format == Format::Local {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} stores its data inside the inode, so writing it is a \
                 metadata write",
                inode.ino
            )));
        }

        let end = offset.checked_add(data.len() as u64).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "inode {}: write range overflows a 64-bit offset",
                inode.ino
            ))
        })?;
        if end > inode.size {
            return Err(Error::UnsupportedFeature(format!(
                "inode {}: writing to {end} would grow the file past its {} bytes, \
                 which changes the inode",
                inode.ino, inode.size
            )));
        }

        // Resolve every destination before writing any of them. A write
        // that discovered a hole halfway through would leave the file
        // half updated with no way to say how far it got.
        let plan = self.plan_in_place_write(inode, raw, offset, data.len())?;

        let mut done = 0usize;
        for Chunk { at, len } in plan {
            device.write_at(at, &data[done..done + len])?;
            done += len;
        }
        device.flush()?;
        Ok(done)
    }

    /// Where each part of an in-place write lands on the device.
    ///
    /// Built in full before any write happens, so the refusals below all
    /// occur while the file is still untouched.
    fn plan_in_place_write(
        &self,
        inode: &Inode,
        raw: &[u8],
        offset: u64,
        len: usize,
    ) -> Result<Vec<Chunk>> {
        let extents = self.data_extents(inode, raw)?;
        let block_size = u64::from(self.sb.blocksize);
        let mut plan = Vec::new();
        let mut done = 0usize;

        while done < len {
            let pos = offset + done as u64;
            let file_block = pos / block_size;
            let within = (pos % block_size) as usize;
            let chunk = (block_size as usize - within).min(len - done);

            let Some(e) = extent::lookup(&extents, file_block) else {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}: file block {file_block} is a hole, and filling it \
                     would allocate blocks",
                    inode.ino
                )));
            };
            if e.is_unwritten() {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}: file block {file_block} is in an unwritten extent, and \
                     writing it would clear that flag in the extent list",
                    inode.ino
                )));
            }

            let phys = e
                .map(file_block)
                .expect("lookup returned an extent covering this block");
            plan.push(Chunk {
                at: self.block_offset(phys) + within as u64,
                len: chunk,
            });
            done += chunk;
        }
        Ok(plan)
    }
}

/// A change to an inode's core fields.
///
/// Every field is optional and `None` means "leave it alone", so a
/// caller changing one thing does not have to read and restate the
/// others — restating them is how a concurrent change gets reverted by
/// a caller that never intended to touch it.
#[derive(Debug, Default, Clone)]
pub struct AttrChange {
    /// Permission bits only. The file-type bits are preserved from the
    /// inode as it stands: changing a file into a directory is not an
    /// attribute change, it is a different filesystem entirely, and
    /// accepting it here would let a caller do it by accident.
    pub permissions: Option<u16>,
    /// Owning user.
    pub uid: Option<u32>,
    /// Owning group.
    pub gid: Option<u32>,
    /// Last access time.
    pub atime: Option<Timestamp>,
    /// Last modification time.
    pub mtime: Option<Timestamp>,
    /// Inode change time. Normally left `None` and set automatically to
    /// the greatest of the times supplied, since it exists to record
    /// when the inode itself last changed — which is now, by definition.
    pub ctime: Option<Timestamp>,
}

impl AttrChange {
    fn is_empty(&self) -> bool {
        self.permissions.is_none()
            && self.uid.is_none()
            && self.gid.is_none()
            && self.atime.is_none()
            && self.mtime.is_none()
            && self.ctime.is_none()
    }
}

/// Permission bits within `di_mode`; everything above them is the type.
const MODE_PERM_MASK: u16 = 0o7777;

impl Filesystem {
    /// Change an inode's timestamps, permissions or ownership.
    ///
    /// # Why this needs no log entry, and what that does not cover
    ///
    /// XFS journals metadata so that a change spanning several
    /// structures either happens completely or not at all. An inode core
    /// field is not such a change: a timestamp or a permission bit
    /// belongs to one inode and is referenced by nothing else. There is
    /// no second structure that must agree with it, so there is no
    /// inconsistent intermediate state for a log to protect against —
    /// unlike an allocation, where two free-space trees must be updated
    /// together, or a rename, which touches two directories and a link
    /// count.
    ///
    /// The mount already refuses a volume whose log holds unapplied
    /// records, so there is also no pending logged version of this inode
    /// that a direct write could overwrite.
    ///
    /// **What it does not cover is a torn write.** The inode is written
    /// as one sector with a recomputed CRC, so a machine that dies
    /// mid-write leaves an inode whose checksum fails, and the volume
    /// then needs repair. Real devices write a sector atomically and
    /// XFS relies on that for its own superblock, so this is a narrow
    /// window — but it is a real one, and it is the window the log
    /// closes. Until the log writer exists, this is a metadata write
    /// that cannot leave the filesystem *inconsistent* but can leave one
    /// inode *unreadable*.
    ///
    /// # Errors
    ///
    /// [`Error::ReadOnly`] unless opened with [`Filesystem::mount_rw`].
    /// [`Error::UnsupportedFeature`] for a v1 or v2 inode, which has no
    /// CRC and predates the fields this writes.
    pub fn set_attributes(&self, inode: &Inode, change: &AttrChange) -> Result<()> {
        let Some(device) = self.writable.as_ref() else {
            return Err(Error::ReadOnly);
        };
        if change.is_empty() {
            return Ok(());
        }
        if inode.version < 3 {
            return Err(Error::UnsupportedFeature(format!(
                "inode {} is version {}, which has no CRC and stores a different core",
                inode.ino, inode.version
            )));
        }
        if let Some(perms) = change.permissions {
            if perms & !MODE_PERM_MASK != 0 {
                return Err(Error::UnsupportedFeature(format!(
                    "inode {}: {perms:#o} sets bits outside the permission mask, which                      would change the file's type",
                    inode.ino
                )));
            }
        }

        // Read the inode afresh rather than trusting a caller-supplied
        // copy: everything not being changed is written back verbatim,
        // so a stale buffer would silently revert whatever else has
        // happened to this inode since it was read.
        let at = self.inode_offset(inode.ino)?;
        let mut raw = vec![0u8; usize::from(self.sb.inodesize)];
        device.read_at(at, &mut raw)?;
        let current = Inode::parse(&raw, &self.sb, inode.ino)?;

        let bigtime = current.flags2 & crate::inode::flags2::BIGTIME != 0;

        if let Some(perms) = change.permissions {
            let kept_type = current.mode & !MODE_PERM_MASK;
            let mode = kept_type | perms;
            raw[inode_offsets::MODE..inode_offsets::MODE + 2].copy_from_slice(&mode.to_be_bytes());
        }
        if let Some(uid) = change.uid {
            raw[inode_offsets::UID..inode_offsets::UID + 4].copy_from_slice(&uid.to_be_bytes());
        }
        if let Some(gid) = change.gid {
            raw[inode_offsets::GID..inode_offsets::GID + 4].copy_from_slice(&gid.to_be_bytes());
        }
        if let Some(t) = change.atime {
            t.encode(&mut raw, inode_offsets::ATIME, bigtime);
        }
        if let Some(t) = change.mtime {
            t.encode(&mut raw, inode_offsets::MTIME, bigtime);
        }

        // `di_ctime` records when the inode last changed, which is now.
        // Taking the caller's value when they gave one, and otherwise the
        // latest time they set, keeps it from being older than the fields
        // it is supposed to be describing the change to.
        if let Some(t) = derived_ctime(change) {
            t.encode(&mut raw, inode_offsets::CTIME, bigtime);
        }

        // The CRC covers the whole inode with its own field zeroed, so it
        // has to be recomputed after every other change and written last.
        let isize_bytes = usize::from(self.sb.inodesize);
        raw[inode_offsets::CRC..inode_offsets::CRC + 4].copy_from_slice(&[0, 0, 0, 0]);
        let crc = crc32c_with_zeroed_crc(&raw[..isize_bytes], inode_offsets::CRC);
        raw[inode_offsets::CRC..inode_offsets::CRC + 4].copy_from_slice(&crc.to_le_bytes());

        device.write_at(at, &raw)?;
        device.flush()?;
        Ok(())
    }
}

/// What `di_ctime` should become for a given change.
///
/// It records when the inode last changed, which is now — so a caller's
/// explicit value wins, and otherwise it takes the latest of the times
/// being set. Leaving it behind the fields it describes would say the
/// inode was modified before its own modification time.
fn derived_ctime(change: &AttrChange) -> Option<Timestamp> {
    if let Some(c) = change.ctime {
        return Some(c);
    }
    match (change.mtime, change.atime) {
        (Some(m), Some(a)) => Some(if m.sec >= a.sec { m } else { a }),
        (Some(m), None) => Some(m),
        (None, Some(a)) => Some(a),
        (None, None) => None,
    }
}

/// One contiguous run of a write, already resolved to a device offset.
struct Chunk {
    at: u64,
    len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::superblock::Superblock;
    use fs_core::BlockDevice;
    use std::sync::{Arc, Mutex};

    /// A writable in-memory device, so the refusals can be exercised
    /// without a fixture and the accepted case can be read back.
    struct MemDev {
        bytes: Mutex<Vec<u8>>,
        writable: bool,
    }

    impl fs_core::BlockRead for MemDev {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
            let b = self.bytes.lock().unwrap();
            let start = offset as usize;
            buf.copy_from_slice(&b[start..start + buf.len()]);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.bytes.lock().unwrap().len() as u64
        }
    }

    impl BlockDevice for MemDev {
        fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
            if !self.writable {
                return Err(fs_core::Error::ReadOnly);
            }
            let mut b = self.bytes.lock().unwrap();
            let start = offset as usize;
            b[start..start + buf.len()].copy_from_slice(buf);
            Ok(())
        }
        fn is_writable(&self) -> bool {
            self.writable
        }
    }

    /// A minimal, well-formed v5 superblock. Every refusal under test is
    /// decided from the inode and the mount mode alone, before an extent
    /// is resolved, so the geometry only has to parse.
    fn superblock() -> Superblock {
        let mut b = vec![0u8; 512];
        b[0..4].copy_from_slice(&crate::superblock::XFS_SB_MAGIC.to_be_bytes());
        b[4..8].copy_from_slice(&4096u32.to_be_bytes()); // blocksize
        b[8..16].copy_from_slice(&4096u64.to_be_bytes()); // dblocks
        b[48..56].copy_from_slice(&4u64.to_be_bytes()); // logstart
        b[56..64].copy_from_slice(&128u64.to_be_bytes()); // rootino
        b[84..88].copy_from_slice(&1024u32.to_be_bytes()); // agblocks
        b[88..92].copy_from_slice(&4u32.to_be_bytes()); // agcount
        b[96..100].copy_from_slice(&16u32.to_be_bytes()); // logblocks
        let versionnum = 5u16 | crate::superblock::version_flags::MOREBITSBIT;
        b[100..102].copy_from_slice(&versionnum.to_be_bytes());
        b[102..104].copy_from_slice(&512u16.to_be_bytes()); // sectsize
        b[104..106].copy_from_slice(&512u16.to_be_bytes()); // inodesize
        b[106..108].copy_from_slice(&8u16.to_be_bytes()); // inopblock
        b[120] = 12; // blocklog
        b[121] = 9; // sectlog
        b[122] = 9; // inodelog
        b[123] = 3; // inopblog
        b[124] = 10; // agblklog
        let crc = crate::superblock::crc32c_with_zeroed_crc(&b, 224);
        b[224..228].copy_from_slice(&crc.to_le_bytes());
        Superblock::parse(&b).expect("superblock")
    }

    /// A filesystem handle over `dev`, opened read-write or not.
    fn fs_over(dev: Arc<MemDev>, writable: bool) -> Filesystem {
        Filesystem {
            device: dev.clone(),
            writable: writable.then_some(dev as Arc<dyn BlockDevice>),
            sb: superblock(),
        }
    }

    fn dev() -> Arc<MemDev> {
        Arc::new(MemDev {
            bytes: Mutex::new(vec![0u8; 1 << 20]),
            writable: true,
        })
    }

    /// A regular file inode with an extent-format data fork. Built
    /// directly rather than parsed: the refusals under test read a
    /// handful of fields, and stating them outright is clearer than
    /// hiding them in a byte buffer.
    fn regular_inode(size: u64) -> Inode {
        Inode {
            ino: 133,
            mode: 0o100644,
            version: 3,
            format: Format::Extents,
            aformat: Format::Local,
            uid: 0,
            gid: 0,
            nlink: 1,
            size,
            nblocks: size.div_ceil(4096),
            nextents: 1,
            anextents: 0,
            forkoff: 0,
            flags: 0,
            flags2: 0,
            gen: 1,
            next_unlinked: u32::MAX,
            atime: epoch(),
            mtime: epoch(),
            ctime: epoch(),
            crtime: epoch(),
        }
    }

    fn epoch() -> crate::inode::Timestamp {
        crate::inode::Timestamp { sec: 0, nsec: 0 }
    }

    #[test]
    fn a_read_only_mount_refuses_to_write() {
        let fs = fs_over(dev(), false);
        let inode = regular_inode(4096);
        let err = fs.write_at(&inode, &[], 0, b"x").unwrap_err();
        assert!(matches!(err, Error::ReadOnly), "got {err}");
    }

    /// The write must be refused before anything reaches the device, not
    /// partway through it.
    #[test]
    fn a_write_past_the_end_of_the_file_is_refused() {
        let d = dev();
        let fs = fs_over(d.clone(), true);
        let inode = regular_inode(10);
        let err = fs.write_at(&inode, &[], 8, b"12345").unwrap_err();
        assert!(format!("{err}").contains("grow the file"), "got {err}");
        assert!(
            d.bytes.lock().unwrap().iter().all(|&b| b == 0),
            "a refused write still touched the device"
        );
    }

    #[test]
    fn a_reflinked_inode_is_refused() {
        let fs = fs_over(dev(), true);
        let mut inode = regular_inode(4096);
        inode.flags2 |= crate::inode::flags2::REFLINK;
        let err = fs.write_at(&inode, &[], 0, b"x").unwrap_err();
        assert!(format!("{err}").contains("reflinked"), "got {err}");
    }

    #[test]
    fn an_inline_file_is_refused() {
        let fs = fs_over(dev(), true);
        let mut inode = regular_inode(16);
        inode.format = Format::Local;
        let err = fs.write_at(&inode, &[], 0, b"x").unwrap_err();
        assert!(format!("{err}").contains("inside the inode"), "got {err}");
    }

    #[test]
    fn a_real_time_inode_is_refused() {
        let fs = fs_over(dev(), true);
        let mut inode = regular_inode(4096);
        inode.flags |= crate::inode::flags::REALTIME;
        let err = fs.write_at(&inode, &[], 0, b"x").unwrap_err();
        assert!(format!("{err}").contains("real-time"), "got {err}");
    }

    #[test]
    fn a_directory_is_not_a_file() {
        let fs = fs_over(dev(), true);
        let mut inode = regular_inode(4096);
        inode.mode = 0o040755;
        let err = fs.write_at(&inode, &[], 0, b"x").unwrap_err();
        assert!(matches!(err, Error::NotAFile), "got {err}");
    }

    fn ts(sec: i64) -> Timestamp {
        Timestamp { sec, nsec: 0 }
    }

    #[test]
    fn a_read_only_mount_refuses_an_attribute_change() {
        let fs = fs_over(dev(), false);
        let inode = regular_inode(4096);
        let change = AttrChange {
            permissions: Some(0o600),
            ..Default::default()
        };
        let err = fs.set_attributes(&inode, &change).unwrap_err();
        assert!(matches!(err, Error::ReadOnly), "got {err}");
    }

    /// A v1 or v2 inode has no CRC and a different core, so writing one
    /// with the v3 layout would corrupt it.
    #[test]
    fn an_older_inode_version_is_refused() {
        let fs = fs_over(dev(), true);
        let mut inode = regular_inode(4096);
        inode.version = 2;
        let change = AttrChange {
            permissions: Some(0o600),
            ..Default::default()
        };
        let err = fs.set_attributes(&inode, &change).unwrap_err();
        assert!(format!("{err}").contains("version 2"), "got {err}");
    }

    /// The type bits are not a permission, and accepting them here would
    /// let a caller turn a file into a directory by arithmetic.
    #[test]
    fn a_mode_carrying_type_bits_is_refused() {
        let fs = fs_over(dev(), true);
        let inode = regular_inode(4096);
        let change = AttrChange {
            permissions: Some(0o040755),
            ..Default::default()
        };
        let err = fs.set_attributes(&inode, &change).unwrap_err();
        assert!(
            format!("{err}").contains("outside the permission mask"),
            "got {err}"
        );
    }

    #[test]
    fn an_empty_attribute_change_touches_nothing() {
        let d = dev();
        let fs = fs_over(d.clone(), true);
        let inode = regular_inode(4096);
        fs.set_attributes(&inode, &AttrChange::default()).unwrap();
        assert!(d.bytes.lock().unwrap().iter().all(|&b| b == 0));
    }

    #[test]
    fn ctime_follows_the_latest_time_being_set() {
        let m = ts(200);
        let a = ts(100);
        assert_eq!(
            derived_ctime(&AttrChange {
                mtime: Some(m),
                atime: Some(a),
                ..Default::default()
            }),
            Some(m),
            "the later of the two should win"
        );
        assert_eq!(
            derived_ctime(&AttrChange {
                mtime: Some(a),
                atime: Some(m),
                ..Default::default()
            }),
            Some(m),
            "and it should not depend on which field it came from"
        );
    }

    #[test]
    fn an_explicit_ctime_wins_over_the_derived_one() {
        let explicit = ts(5);
        assert_eq!(
            derived_ctime(&AttrChange {
                mtime: Some(ts(999)),
                ctime: Some(explicit),
                ..Default::default()
            }),
            Some(explicit)
        );
    }

    /// A change that sets no time leaves ctime alone rather than
    /// inventing one — this driver has no clock it should be trusting.
    #[test]
    fn a_permissions_only_change_derives_no_ctime() {
        assert_eq!(
            derived_ctime(&AttrChange {
                permissions: Some(0o600),
                ..Default::default()
            }),
            None
        );
    }

    /// An empty write is a no-op rather than an error, and must not be
    /// refused by any of the checks above — a caller looping over chunks
    /// should not have to special-case the last one.
    #[test]
    fn an_empty_write_succeeds_without_touching_anything() {
        let d = dev();
        let fs = fs_over(d.clone(), true);
        let inode = regular_inode(4096);
        assert_eq!(fs.write_at(&inode, &[], 0, b"").unwrap(), 0);
        assert!(d.bytes.lock().unwrap().iter().all(|&b| b == 0));
    }
}
