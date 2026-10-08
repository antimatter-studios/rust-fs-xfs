//! In-memory accounting edge cases; kernel fixtures validate the format separately.

use super::*;
use crate::check::{Checker, Report};
use crate::{Filesystem, Superblock};
use fs_core::BlockRead;
use std::sync::Arc;

struct Device {
    sector: Vec<u8>,
    quotas: Vec<u8>,
}
impl BlockRead for Device {
    fn size_bytes(&self) -> u64 {
        400 << 20
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        buf.fill(0);
        for (start, data) in [(0, &self.sector), (2000 * 4096, &self.quotas)] {
            let first = offset.max(start);
            let last = (offset + buf.len() as u64).min(start + data.len() as u64);
            if first < last {
                buf[(first - offset) as usize..(last - offset) as usize]
                    .copy_from_slice(&data[(first - start) as usize..(last - start) as usize]);
            }
        }
        Ok(())
    }
}

fn model() -> (Filesystem, BTreeMap<u64, Inode>, HashMap<u64, Vec<u8>>) {
    let mut sb: Superblock = crate::mkfs::plan(400 << 20, &crate::mkfs::Options::default())
        .unwrap()
        .superblock()
        .clone();
    sb.qflags = 5;
    sb.uquotino = 999;
    sb.gquotino = u64::MAX;
    sb.pquotino = u64::MAX;
    let mut sector = vec![0; 512];
    crate::super_write::apply(&mut sector, &sb).unwrap();
    let mut quotas = vec![0; 4096];
    for id in 0..30u32 {
        let b = &mut quotas[id as usize * RECORD_SIZE..(id as usize + 1) * RECORD_SIZE];
        b[..2].copy_from_slice(&0x4451u16.to_be_bytes());
        b[2] = 1;
        b[3] = 1;
        b[4..8].copy_from_slice(&id.to_be_bytes());
        if id == 17 {
            b[40..48].copy_from_slice(&4u64.to_be_bytes());
            b[48..56].copy_from_slice(&1u64.to_be_bytes());
        }
        b[120..136].copy_from_slice(&sb.meta_uuid);
        let crc = crate::superblock::crc32c_with_zeroed_crc(b, 108);
        b[108..112].copy_from_slice(&crc.to_le_bytes());
    }
    let fs = Filesystem {
        device: Arc::new(Device { sector, quotas }),
        sb,
        writable: None,
        replayed: false,
        overlay: None,
        oldest_record: std::sync::Mutex::new(None),
        next_head: std::sync::Mutex::new(None),
        wraps: std::sync::atomic::AtomicUsize::new(0),
        logged_anything: std::sync::atomic::AtomicBool::new(false),
        realtime: None,
    };
    let time = crate::inode::Timestamp { sec: 0, nsec: 0 };
    let quota = Inode {
        ino: 999,
        mode: 0o100000,
        version: 3,
        format: Format::Extents,
        aformat: Format::Extents,
        uid: 0,
        gid: 0,
        nlink: 1,
        size: 0,
        nblocks: 1,
        nextents: 1,
        anextents: 0,
        forkoff: 0,
        flags: 0,
        flags2: 0,
        gen: 0,
        next_unlinked: u32::MAX,
        atime: time,
        mtime: time,
        ctime: time,
        crtime: time,
    };
    let mut file = quota.clone();
    file.ino = 1000;
    file.uid = 17;
    file.nblocks = 4;
    file.nextents = 0;
    let mut raw = vec![0; 512];
    let (start, _) = quota.data_fork_range(512);
    // One data block at physical block 2000, logical block zero.
    raw[start..start + 16].copy_from_slice(
        &crate::extent::Extent {
            startoff: 0,
            startblock: 2000,
            blockcount: 1,
            unwritten: false,
        }
        .to_bytes()
        .unwrap(),
    );
    (
        fs,
        BTreeMap::from([(999, quota), (1000, file)]),
        HashMap::from([(999, raw), (1000, vec![0; 512])]),
    )
}

fn run(
    fs: &Filesystem,
    inodes: &BTreeMap<u64, Inode>,
    raws: &HashMap<u64, Vec<u8>>,
    allocated: &[u64],
) -> Report {
    let mut c = Checker {
        fs,
        report: Report::default(),
        owners: Vec::new(),
        shared: Vec::new(),
        counted: HashMap::new(),
        allocated: allocated.iter().copied().collect(),
        free: HashSet::new(),
    };
    c.quotas(inodes, raws);
    c.report
}

#[test]
fn accounting_requires_a_complete_checked_walk_and_excludes_quota_storage() {
    let (mut fs, mut inodes, raws) = model();
    assert!(run(&fs, &inodes, &raws, &[999, 1000]).is_clean());
    inodes.get_mut(&1000).unwrap().nblocks = 5;
    let found = run(&fs, &inodes, &raws, &[999, 1000]);
    assert_eq!(found.findings.len(), 1);
    assert!(found.findings[0]
        .what
        .contains("record 17 counts 4 blocks; allocated inodes account for 5"));
    assert!(run(&fs, &inodes, &raws, &[999, 1000, 1001]).is_clean());
    fs.sb.qflags = 1;
    assert!(run(&fs, &inodes, &raws, &[999, 1000]).is_clean());
}

#[test]
fn missing_duplicate_and_invalid_quota_inode_references_are_findings() {
    let (mut fs, mut inodes, raws) = model();
    fs.sb.gquotino = 999;
    assert!(run(&fs, &inodes, &raws, &[999, 1000])
        .findings
        .iter()
        .any(|f| f.what.contains("another quota type")));
    fs.sb.gquotino = 998;
    assert!(run(&fs, &inodes, &raws, &[999, 1000])
        .findings
        .iter()
        .any(|f| f.what.contains("not a readable allocated inode")));
    fs.sb.gquotino = u64::MAX;
    inodes.get_mut(&999).unwrap().nlink = 2;
    assert!(run(&fs, &inodes, &raws, &[999, 1000])
        .findings
        .iter()
        .any(|f| f.what.contains("invalid type")));
    fs.sb.uquotino = 0;
    assert!(run(&fs, &inodes, &raws, &[999, 1000])
        .findings
        .iter()
        .any(|f| f.what.contains("quota inode is missing")));
    fs.sb.qflags = 0x8000;
    assert!(run(&fs, &inodes, &raws, &[999, 1000])
        .findings
        .iter()
        .any(|f| f.what.contains("quota flags")));
}
