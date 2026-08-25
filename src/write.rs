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
use crate::inode::{Format, Inode};
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
