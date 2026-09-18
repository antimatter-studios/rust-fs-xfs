//! What this mount has logged but nothing has replayed (#89).
//!
//! A journalled operation writes a record and touches nothing on disk. That
//! is what makes each one checkable against the kernel — a filesystem that
//! came out different is one something replayed — and it is why a second
//! operation could not be built: it would read the disk the first one read,
//! as though the first had never happened. Two creates would hand out one
//! inode; a truncate and then an allocation would hand out blocks the first
//! had freed only in the record.
//!
//! This is the missing piece: every buffer a record carries is kept here,
//! and the mount reads through it. The disk is untouched, the records are
//! the truth, and the mount's view agrees with what a replay would arrive
//! at — so each operation is built on the one before it.
//!
//! What is kept is what recovery would write: whole buffers at their device
//! offsets. It is not a cache with a policy; nothing is ever evicted,
//! because dropping an entry would mean reading a stale disk again.

use fs_core::{BlockRead, Result};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// The unit everything here is keyed by: XFS's basic block, and the unit a
/// buffer log item counts in.
const BBSIZE: usize = 512;

/// The device, plus what this mount has logged over it.
pub(crate) struct Overlay {
    inner: Arc<dyn BlockRead>,
    /// Basic-block offset in bytes, to that block's bytes as the last
    /// record left them.
    logged: Mutex<BTreeMap<u64, [u8; BBSIZE]>>,
}

impl Overlay {
    pub(crate) fn new(inner: Arc<dyn BlockRead>) -> Self {
        Overlay {
            inner,
            logged: Mutex::new(BTreeMap::new()),
        }
    }

    /// Record that `bytes` now belong at `offset`, as a record just said.
    ///
    /// # Panics
    ///
    /// If `offset` is not a whole basic block, which no buffer's address
    /// is: a buffer log item counts in basic blocks and so does an inode's.
    pub(crate) fn wrote(&self, offset: u64, bytes: &[u8]) {
        assert!(
            offset.is_multiple_of(BBSIZE as u64),
            "a logged buffer starts on a basic block, not at {offset}"
        );
        let mut logged = self.logged.lock().expect("overlay poisoned");
        for (i, chunk) in bytes.chunks(BBSIZE).enumerate() {
            let at = offset + (i * BBSIZE) as u64;
            // A partial tail is possible for an inode smaller than a basic
            // block; keep what the disk holds around it.
            let entry = logged.entry(at).or_insert_with(|| {
                let mut block = [0u8; BBSIZE];
                let _ = self.inner.read_at(at, &mut block);
                block
            });
            entry[..chunk.len()].copy_from_slice(chunk);
        }
    }

    /// Whether anything has been logged over the disk yet.
    pub(crate) fn is_empty(&self) -> bool {
        self.logged.lock().expect("overlay poisoned").is_empty()
    }
}

impl BlockRead for Overlay {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_at(offset, buf)?;
        if buf.is_empty() {
            return Ok(());
        }
        let logged = self.logged.lock().expect("overlay poisoned");
        let first = offset - offset % BBSIZE as u64;
        let end = offset + buf.len() as u64;
        for (&at, block) in logged.range(first..end) {
            // The overlap between this basic block and the read.
            let from = at.max(offset);
            let to = (at + BBSIZE as u64).min(end);
            if from >= to {
                continue;
            }
            let in_block = (from - at) as usize..(to - at) as usize;
            let in_buf = (from - offset) as usize..(to - offset) as usize;
            buf[in_buf].copy_from_slice(&block[in_block]);
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes in memory, standing in for a device.
    struct Disk(Vec<u8>);

    impl BlockRead for Disk {
        fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
            let at = offset as usize;
            buf.copy_from_slice(&self.0[at..at + buf.len()]);
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn device(bytes: Vec<u8>) -> Arc<dyn BlockRead> {
        Arc::new(Disk(bytes))
    }

    /// A read that has nothing logged over it is the disk's.
    #[test]
    fn an_empty_overlay_reads_the_disk() {
        let overlay = Overlay::new(device(vec![7u8; 4096]));
        let mut buf = [0u8; 8];
        overlay.read_at(16, &mut buf).expect("read");
        assert_eq!(buf, [7u8; 8]);
        assert!(overlay.is_empty());
    }

    /// What was logged is what comes back, and only where it was logged.
    #[test]
    fn a_logged_buffer_is_read_back_in_place_of_the_disk() {
        let overlay = Overlay::new(device(vec![7u8; 4096]));
        overlay.wrote(512, &[9u8; 1024]);

        let mut buf = vec![0u8; 2048];
        overlay.read_at(0, &mut buf).expect("read");
        assert!(buf[..512].iter().all(|&b| b == 7), "before it, the disk");
        assert!(buf[512..1536].iter().all(|&b| b == 9), "the logged bytes");
        assert!(buf[1536..].iter().all(|&b| b == 7), "after it, the disk");
    }

    /// A read that starts inside a logged block still sees it.
    #[test]
    fn a_read_overlapping_one_end_of_a_logged_block_sees_it() {
        let overlay = Overlay::new(device(vec![7u8; 4096]));
        overlay.wrote(1024, &[3u8; 512]);

        let mut buf = vec![0u8; 16];
        overlay.read_at(1020, &mut buf).expect("read");
        assert!(buf[..4].iter().all(|&b| b == 7));
        assert!(buf[4..].iter().all(|&b| b == 3));

        let mut tail = vec![0u8; 16];
        overlay.read_at(1528, &mut tail).expect("read");
        assert!(tail[..8].iter().all(|&b| b == 3));
        assert!(tail[8..].iter().all(|&b| b == 7));
    }

    /// A later record over the same block wins, and a partial write keeps
    /// what was there around it.
    #[test]
    fn a_second_write_replaces_only_what_it_covers() {
        let overlay = Overlay::new(device(vec![7u8; 4096]));
        overlay.wrote(0, &[1u8; 512]);
        overlay.wrote(0, &[2u8; 8]);

        let mut buf = vec![0u8; 512];
        overlay.read_at(0, &mut buf).expect("read");
        assert!(buf[..8].iter().all(|&b| b == 2), "the second write");
        assert!(buf[8..].iter().all(|&b| b == 1), "the first, around it");
    }
}
