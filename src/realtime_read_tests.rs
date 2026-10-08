//! In-memory boundary tests; the independent bytes live in realtime_oracle.
use super::*;
use crate::inode::{flags, Timestamp};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Probe {
    size: u64,
    base: u8,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl Probe {
    fn new(size: u64, base: u8) -> Arc<Self> {
        Arc::new(Self {
            size,
            base,
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
        })
    }
}

impl BlockRead for Probe {
    fn size_bytes(&self) -> u64 {
        self.size
    }
    fn read_at(&self, at: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if at
            .checked_add(buf.len() as u64)
            .is_none_or(|end| end > self.size)
        {
            return Err(fs_core::Error::Io(std::io::Error::other("short device")));
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.base.wrapping_add(((at + i as u64) / 4096) as u8);
        }
        Ok(())
    }
}

impl BlockDevice for Probe {
    fn write_at(&self, _: u64, _: &[u8]) -> fs_core::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        Err(fs_core::Error::ReadOnly)
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn filesystem() -> (Filesystem, Arc<Probe>, Arc<Probe>) {
    // v4 geometry isolates extent addressing from CRC and log recovery.
    let mut b = vec![0; 512];
    for (at, value) in [
        (0, crate::superblock::XFS_SB_MAGIC),
        (4, 4096),
        (80, 1),
        (84, 1000),
        (88, 4),
        (92, 1),
        (96, 200),
    ] {
        b[at..at + 4].copy_from_slice(&value.to_be_bytes());
    }
    for (at, value) in [
        (8, 4000u64),
        (16, 16),
        (24, 16),
        (48, 100),
        (56, 128),
        (64, 129),
        (72, 130),
    ] {
        b[at..at + 8].copy_from_slice(&value.to_be_bytes());
    }
    for (at, value) in [(100, 4u16), (102, 512), (104, 512), (106, 8)] {
        b[at..at + 2].copy_from_slice(&value.to_be_bytes());
    }
    b[120..125].copy_from_slice(&[12, 9, 9, 3, 10]);
    b[125] = 4;
    let sb = Superblock::parse(&b).unwrap();
    let data = Probe::new(4000 * 4096, 0x40);
    let rt = Probe::new(16 * 4096, 0x80);
    let fs = Filesystem {
        device: data.clone(),
        writable: None,
        sb,
        replayed: false,
        overlay: None,
        oldest_record: Mutex::new(None),
        next_head: Mutex::new(None),
        wraps: AtomicUsize::new(0),
        logged_anything: std::sync::atomic::AtomicBool::new(false),
        realtime: Some(rt.clone()),
    };
    (fs, data, rt)
}

fn file(extents: &[Extent], blocks: u64) -> (Inode, Vec<u8>) {
    let zero = Timestamp { sec: 0, nsec: 0 };
    let inode = Inode {
        ino: 144,
        mode: 0o100644,
        version: 2,
        format: Format::Extents,
        aformat: Format::Extents,
        uid: 0,
        gid: 0,
        nlink: 1,
        size: blocks * 4096,
        nblocks: blocks,
        nextents: extents.len() as u64,
        anextents: 0,
        forkoff: 0,
        flags: flags::REALTIME,
        flags2: 0,
        gen: 1,
        next_unlinked: u32::MAX,
        atime: zero,
        mtime: zero,
        ctime: zero,
        crtime: zero,
    };
    let mut raw = vec![0; 512];
    for (i, extent) in extents.iter().enumerate() {
        let at = crate::inode::XFS_DINODE_V2_SIZE + i * 16;
        raw[at..at + 16].copy_from_slice(&extent.to_bytes().unwrap());
    }
    (inode, raw)
}

fn extent(off: u64, start: u64, count: u64, unwritten: bool) -> Extent {
    Extent {
        startoff: off,
        startblock: start,
        blockcount: count,
        unwritten,
    }
}

#[test]
fn a_read_rejects_the_whole_realtime_extent_even_when_the_requested_prefix_fits() {
    let (fs, data, rt) = filesystem();
    for e in [
        extent(0, 15, 2, false),
        extent(0, 16, 1, true),
        extent(2, 16, 1, false),
    ] {
        let (inode, raw) = file(&[e], 4);
        let mut buf = [0xcc; 7];
        let result = fs.read_at(&inode, &raw, 0, &mut buf);
        assert!(
            matches!(result, Err(Error::BadSuperblock(_))),
            "invalid realtime extent {e:?} read successfully: {result:?}"
        );
        assert_eq!(
            buf, [0xcc; 7],
            "a rejected map must not modify the destination"
        );
    }
    assert_eq!(data.reads.load(Ordering::Relaxed), 0);
    assert_eq!(rt.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn realtime_block_zero_and_last_block_read_from_the_second_device() {
    let (fs, data, rt) = filesystem();
    let (inode, raw) = file(&[extent(0, 0, 1, false), extent(1, 15, 1, false)], 2);
    let mut buf = [0; 5];
    assert_eq!(fs.read_at(&inode, &raw, 4094, &mut buf).unwrap(), 5);
    assert_eq!(buf, [0x80, 0x80, 0x8f, 0x8f, 0x8f]);
    assert_eq!(data.reads.load(Ordering::Relaxed), 0);
    assert_eq!(rt.reads.load(Ordering::Relaxed), 2);
    assert_eq!(
        data.writes.load(Ordering::Relaxed) + rt.writes.load(Ordering::Relaxed),
        0
    );
    assert_eq!(fs.sync(), Err(Error::ReadOnly));
}

#[test]
fn ordinary_extents_keep_using_the_data_device() {
    let (fs, data, rt) = filesystem();
    let (mut inode, raw) = file(&[extent(0, 8, 1, false)], 1);
    inode.flags = 0;
    let mut buf = [0; 4];
    fs.read_at(&inode, &raw, 0, &mut buf).unwrap();
    assert_eq!(buf, [0x48; 4]);
    assert_eq!(data.reads.load(Ordering::Relaxed), 1);
    assert_eq!(rt.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn missing_realtime_device_names_the_inode_without_falling_back() {
    let (mut fs, data, rt) = filesystem();
    fs.realtime = None;
    let (inode, raw) = file(&[extent(0, 0, 1, false)], 1);
    assert_eq!(
        fs.read_at(&inode, &raw, 0, &mut [0; 4]),
        Err(Error::RealtimeDeviceAbsent { ino: inode.ino })
    );
    assert_eq!(
        data.reads.load(Ordering::Relaxed) + rt.reads.load(Ordering::Relaxed),
        0
    );
}

#[test]
fn realtime_holes_unwritten_extents_and_eof_do_not_read_stale_bytes() {
    let (fs, data, rt) = filesystem();
    let (inode, raw) = file(&[extent(1, 3, 1, true), extent(2, 4, 1, false)], 3);
    let mut buf = vec![0xff; 3 * 4096 + 5];
    assert_eq!(fs.read_at(&inode, &raw, 0, &mut buf).unwrap(), 3 * 4096);
    assert!(buf[..2 * 4096].iter().all(|b| *b == 0));
    assert!(buf[2 * 4096..3 * 4096].iter().all(|b| *b == 0x84));
    assert_eq!(&buf[3 * 4096..], &[0xff; 5]);
    assert_eq!(fs.read_at(&inode, &raw, inode.size, &mut buf).unwrap(), 0);
    assert_eq!(data.reads.load(Ordering::Relaxed), 0);
    assert_eq!(rt.reads.load(Ordering::Relaxed), 1);
}
