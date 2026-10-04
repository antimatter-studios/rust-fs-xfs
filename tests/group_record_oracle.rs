//! A write refuses a group-tree record the kernel would refuse (#314):
//! a by-length free-space tree that disagrees with the by-block one, and
//! an inode chunk that does not start on a chunk boundary inside its group.
//!
//! The group trees' blocks are verified on every path that writes: magic,
//! CRC, UUID, owner, address and level. The records inside them were used
//! as they stood.
//!
//! - **By-length free space.** `GroupAlloc::open` read the by-length tree
//!   for its blocks and threw its records away, then laid the tree out
//!   again from the by-block records. The two trees describe the same free
//!   space, and the kernel's `xfs_repair` reports a run "only seen by one
//!   free space btree". A misread in one of them was never seen, and the
//!   allocation that rewrote both buried it.
//! - **Inode chunk start.** `xfs_inobt_check_irec` requires the chunk's
//!   first and last inode inside the group (`xfs_verify_agino`), and
//!   `xfs_repair` requires the start on a chunk boundary. A create takes
//!   `startino + n` as the new inode's number, so a chunk record that is
//!   off by one, or names the group's headers, or a block past the group,
//!   builds the new file on a slot the chunk does not own.
//!
//! The volume is a fresh `mkfs.xfs` fixture. Group 0's one-leaf trees are
//! damaged one record at a time, with the leaf's checksum recomputed so the
//! block is internally perfect and only the record is wrong. `xfs_repair -n`
//! judges that each damage is corruption, and the driver has to refuse the
//! operation that consumes the record before it asks the device to write.

mod common;

use common::{oracle, repair, scratch};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Where this suite's scratch volumes live, under `.vm-share/scratch/`,
/// out of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "group_record_oracle";

/// A fresh volume, so group 0's trees are one leaf each.
const FIXTURE: &str = "xfs-default.img";

/// The v5 short-form B+tree block header: the CRC's offset and the length.
const BT_CRC: usize = 52;
const BT_HEADER: usize = 56;
/// `bb_level` and `bb_numrecs`.
const BT_LEVEL: usize = 4;
const BT_NUMRECS: usize = 6;

/// A free-space record.
const FREE_RECORD: usize = 8;

/// `agi_root` in the AGI.
const AGI_ROOT: usize = 20;

/// The operation a damage is judged under.
#[derive(Clone, Copy)]
enum Consumer {
    /// `write_into_empty_file`, which takes blocks from the free-space
    /// trees and lays both out again.
    Allocation,
    /// `create_file`, which takes an inode from a chunk record.
    Create,
}

/// What a damage does, and to which tree.
struct Damage {
    what: &'static str,
    consumer: Consumer,
    /// Rewrite the leaf's records in place, given the volume's geometry.
    apply: fn(&mut [u8], &Geometry),
}

struct Geometry {
    /// Group 0's length in blocks.
    length: u32,
    /// `sb_inopblog`.
    inopblog: u8,
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_be_bytes());
}

fn numrecs(leaf: &[u8]) -> usize {
    usize::from(u16::from_be_bytes(
        leaf[BT_NUMRECS..BT_NUMRECS + 2].try_into().unwrap(),
    ))
}

const DAMAGES: [Damage; 4] = [
    Damage {
        what: "the by-length tree's longest run is one block shorter than the by-block tree's",
        consumer: Consumer::Allocation,
        apply: |leaf, _| {
            let last = BT_HEADER + (numrecs(leaf) - 1) * FREE_RECORD;
            let count = be32(leaf, last + 4);
            assert!(count > 1, "the longest free run is a single block");
            put32(leaf, last + 4, count - 1);
        },
    },
    Damage {
        what: "the first inode chunk starts one inode past a chunk boundary",
        consumer: Consumer::Create,
        apply: |leaf, _| {
            let start = be32(leaf, BT_HEADER);
            put32(leaf, BT_HEADER, start + 1);
        },
    },
    Damage {
        what: "the first inode chunk starts on the group's headers",
        consumer: Consumer::Create,
        apply: |leaf, _| put32(leaf, BT_HEADER, 0),
    },
    Damage {
        what: "the first inode chunk starts past the end of the group",
        consumer: Consumer::Create,
        apply: |leaf, g| put32(leaf, BT_HEADER, g.length << g.inopblog),
    },
];

/// A writable device that counts what it is asked to write.
struct Watched {
    inner: FileDevice,
    writes: AtomicUsize,
}

impl BlockRead for Watched {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

impl BlockDevice for Watched {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.inner.write_at(offset, buf)
    }
    fn flush(&self) -> fs_core::Result<()> {
        self.inner.flush()
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn fresh_copy(tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &common::fixture(FIXTURE),
        &format!("{tag}-{}.img", std::process::id()),
    )
}

/// Where group 0's by-length leaf and inode-tree leaf are, how big a block
/// is, and the volume's geometry.
struct Leaves {
    cnt: u64,
    inobt: u64,
    blocksize: usize,
    geometry: Geometry,
}

fn the_leaves(image: &Path) -> Leaves {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    let sb = fs.superblock();
    let blocksize = sb.blocksize as usize;
    let sectsize = u64::from(sb.sectsize);
    let mut f = std::fs::File::open(image).expect("open the volume");
    let mut sector = |at: u64| {
        let mut b = vec![0u8; sectsize as usize];
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut b).unwrap();
        b
    };
    let agi = sector(sectsize * 2);
    let agf = fs.agf(0).expect("group 0's AGF");
    let leaves = Leaves {
        // `agf_roots[1]`, the by-length tree's.
        cnt: u64::from(agf.roots[1]) * blocksize as u64,
        inobt: u64::from(be32(&agi, AGI_ROOT)) * blocksize as u64,
        blocksize,
        geometry: Geometry {
            length: agf.length,
            inopblog: sb.inopblog,
        },
    };
    for (what, at) in [("by-length", leaves.cnt), ("inode", leaves.inobt)] {
        let mut b = vec![0u8; blocksize];
        f.seek(SeekFrom::Start(at)).unwrap();
        f.read_exact(&mut b).unwrap();
        assert_eq!(
            u16::from_be_bytes(b[BT_LEVEL..BT_LEVEL + 2].try_into().unwrap()),
            0,
            "group 0's {what} tree must be one leaf for this test to damage it"
        );
        assert!(numrecs(&b) > 0, "group 0's {what} leaf has no records");
    }
    leaves
}

/// Apply `d` to the leaf it names, and recompute the leaf's checksum.
fn damage(image: &Path, leaves: &Leaves, d: &Damage) {
    let at = match d.consumer {
        Consumer::Allocation => leaves.cnt,
        Consumer::Create => leaves.inobt,
    };
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut block = vec![0u8; leaves.blocksize];
    f.read_exact(&mut block).unwrap();

    (d.apply)(&mut block, &leaves.geometry);

    block[BT_CRC..BT_CRC + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c::crc32c(&block);
    block[BT_CRC..BT_CRC + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
}

/// Run the operation `consumer` names and say whether it went ahead: an
/// `Ok`, or any device write it made.
fn it_went_ahead(image: &Path, consumer: Consumer) -> Option<String> {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged group-tree record does not stop the mount");
    let root = fs.superblock().rootino;
    match consumer {
        Consumer::Create => {
            let before = dev.writes.load(Ordering::SeqCst);
            let result = fs.create_file(root, b"new", 0o100644);
            let writes = dev.writes.load(Ordering::SeqCst) - before;
            (result.is_ok() || writes != 0)
                .then(|| format!("create_file returned {result:?} after {writes} writes"))
        }
        Consumer::Allocation => {
            let (ino, _) = fs
                .create_file(root, b"new", 0o100644)
                .expect("the create takes an inode, not blocks");
            let before = dev.writes.load(Ordering::SeqCst);
            let blocksize = fs.superblock().blocksize as usize;
            let result = fs.write_into_empty_file(ino, &vec![0xab; blocksize]);
            let writes = dev.writes.load(Ordering::SeqCst) - before;
            (result.is_ok() || writes != 0)
                .then(|| format!("write_into_empty_file returned {result:?} after {writes} writes"))
        }
    }
}

/// The driver refuses every damage and writes nothing for it.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn a_write_refuses_a_group_tree_record_the_kernel_would_refuse() {
    let probe = fresh_copy("probe");
    let leaves = the_leaves(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("driver-{i}"));
        damage(volume.path(), &leaves, d);
        if let Some(what) = it_went_ahead(volume.path(), d.consumer) {
            wrong.push(format!("{}: {what}", d.what));
        }
    }
    assert!(
        wrong.is_empty(),
        "a write went ahead on a group-tree record the kernel refuses:\n  {}",
        wrong.join("\n  ")
    );

    // The undamaged volume still takes both operations, so the refusals
    // above are about the damage and not about the volume.
    for consumer in [Consumer::Create, Consumer::Allocation] {
        let volume = fresh_copy("driver-undamaged");
        assert!(
            it_went_ahead(volume.path(), consumer).is_some(),
            "the undamaged volume takes the operation"
        );
    }
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_damaged_record_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let leaves = the_leaves(clean.path());
    // Graded by repair::assert_agreed rather than the exit status alone: a
    // clean exit beside a note that the log was ignored is the tool
    // declining to look (#124).
    let judged = oracle("xfs_repair").arg("-n").arg(clean.path()).output();
    repair::assert_agreed(
        &judged.repair_report(),
        "the undamaged copy must be clean to xfs_repair -n, or nothing below means anything",
    );
    drop(clean);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("oracle-{i}"));
        damage(volume.path(), &leaves, d);
        let judged = oracle("xfs_repair").arg("-n").arg(volume.path()).output();
        // A complaint from a tool that ignored the log is not a verdict
        // either, so it cannot stand as the corruption this needs.
        if repair::was_blind(&judged.repair_report()) {
            wrong.push(format!(
                "{}: xfs_repair -n ignored the log, so its report judges nothing:\n{}",
                d.what,
                judged.repair_report()
            ));
        } else if judged.ok() {
            wrong.push(format!(
                "{}: xfs_repair -n called it clean, so this is not a damage the driver \
                 should refuse:\n{}",
                d.what,
                judged.repair_report()
            ));
        }
        drop(volume);
        let volume = fresh_copy(&format!("oracle-{i}-driver"));
        damage(volume.path(), &leaves, d);
        if let Some(what) = it_went_ahead(volume.path(), d.consumer) {
            wrong.push(format!(
                "{}: xfs_repair -n reports corruption, and {what}",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
