//! An allocation refuses a free-space record that does not describe free
//! space inside its group (#314).
//!
//! The by-block free-space tree's blocks are verified on every path that
//! writes: magic, CRC, UUID, owner, address and level. The records inside
//! them were decoded and used as they stood. `GroupAlloc::take` hands out
//! the first run long enough, so a record that names the group's headers,
//! or runs past the group's end, or overlaps the record before it, is
//! handed out as free. `write_into_empty_file` then writes the file's bytes
//! there before it writes the record that claims them.
//!
//! The kernel checks every record as it reads it. `xfs_alloc_check_irec`
//! requires a nonzero length and `xfs_verify_agbext`: the run starts past
//! `XFS_AGFL_BLOCK` and ends inside the group. The B+tree code requires the
//! records in ascending order.
//!
//! The volume is a fresh `mkfs.xfs` fixture. Group 0's by-block leaf is
//! damaged one record at a time, with its checksum recomputed so the block
//! is internally perfect and only the record is wrong. `xfs_repair -n`
//! judges that each damage is corruption. The driver creates an empty file
//! and writes into it, which takes blocks from group 0, and the write has
//! to be refused before it asks the device for anything.

mod common;

use common::{oracle, repair, scratch};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "free_space_record_oracle";

/// A fresh volume, so group 0's by-block tree is one leaf.
const FIXTURE: &str = "xfs-default.img";

/// The v5 short-form B+tree block header: the CRC's offset and the length.
const BT_CRC: usize = 52;
const BT_HEADER: usize = 56;
/// `bb_numrecs`.
const BT_NUMRECS: usize = 6;

/// What a damage does to the leaf's records, given the group's length.
struct Damage {
    what: &'static str,
    /// Rewrite the records `(startblock, blockcount)`.
    apply: fn(&mut Vec<(u32, u32)>, u32),
}

const DAMAGES: [Damage; 3] = [
    Damage {
        what: "the first free run is stretched back over the group's headers",
        apply: |r, _| {
            let (start, count) = r[0];
            r[0] = (0, start + count);
        },
    },
    Damage {
        what: "the first free run is moved past the end of the group",
        apply: |r, length| {
            let (_, count) = r[0];
            r[0] = (length - 1, count);
        },
    },
    Damage {
        what: "a second record overlaps the first",
        apply: |r, _| {
            let (start, count) = r[0];
            r.insert(1, (start + count / 2, count));
            if r.len() > 2 {
                r.truncate(2);
            }
        },
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

/// Where group 0's by-block leaf is, how big a block is, and the group's
/// length.
fn the_leaf(image: &Path) -> (u64, usize, u32) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    let agf = fs.agf(0).expect("group 0's AGF");
    assert_eq!(
        agf.levels[0], 1,
        "group 0's by-block tree must be one leaf for this test to damage it"
    );
    let blocksize = fs.superblock().blocksize;
    (
        u64::from(agf.roots[0]) * u64::from(blocksize),
        blocksize as usize,
        agf.length,
    )
}

/// Rewrite the leaf's records as `d` says, and recompute its checksum.
fn damage(image: &Path, at: u64, blocksize: usize, length: u32, d: &Damage) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut block = vec![0u8; blocksize];
    f.read_exact(&mut block).unwrap();

    let be32 = |b: &[u8], at: usize| u32::from_be_bytes(b[at..at + 4].try_into().unwrap());
    let n = usize::from(u16::from_be_bytes(
        block[BT_NUMRECS..BT_NUMRECS + 2].try_into().unwrap(),
    ));
    let mut records: Vec<(u32, u32)> = (0..n)
        .map(|i| {
            let at = BT_HEADER + i * 8;
            (be32(&block, at), be32(&block, at + 4))
        })
        .collect();
    (d.apply)(&mut records, length);
    block[BT_NUMRECS..BT_NUMRECS + 2].copy_from_slice(&(records.len() as u16).to_be_bytes());
    for (i, (start, count)) in records.iter().enumerate() {
        let at = BT_HEADER + i * 8;
        block[at..at + 4].copy_from_slice(&start.to_be_bytes());
        block[at + 4..at + 8].copy_from_slice(&count.to_be_bytes());
    }

    block[BT_CRC..BT_CRC + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c::crc32c(&block);
    block[BT_CRC..BT_CRC + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
}

/// Create an empty file, then write one block into it, and say whether the
/// write went ahead: an `Ok`, or any device write after the create's.
fn the_write_went_ahead(image: &Path) -> Option<String> {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged free-space record does not stop the mount");
    let root = fs.superblock().rootino;
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

/// The driver refuses every damage and writes nothing for it.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn an_allocation_refuses_a_record_that_is_not_free_space_in_its_group() {
    let probe = fresh_copy("probe");
    let (at, blocksize, length) = the_leaf(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("driver-{i}"));
        damage(volume.path(), at, blocksize, length, d);
        if let Some(what) = the_write_went_ahead(volume.path()) {
            wrong.push(format!("{}: {what}", d.what));
        }
    }
    assert!(
        wrong.is_empty(),
        "an allocation went ahead on a record that is not free space in its group:\n  {}",
        wrong.join("\n  ")
    );

    // The undamaged leaf still gives the write its block, so the refusals
    // above are about the damage and not about the volume.
    let volume = fresh_copy("driver-undamaged");
    assert!(
        the_write_went_ahead(volume.path()).is_some(),
        "the undamaged volume takes the write"
    );
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_damaged_record_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let (at, blocksize, length) = the_leaf(clean.path());
    // The host path, which is inside the repository and so is the same
    // path in the guest. Graded by repair::assert_agreed rather than the
    // exit status alone: a clean exit beside a note that the log was
    // ignored is the tool declining to look (#124).
    let judged = oracle("xfs_repair").arg("-n").arg(clean.path()).output();
    repair::assert_agreed(
        &judged.repair_report(),
        "the undamaged copy must be clean to xfs_repair -n, or nothing below means anything",
    );
    drop(clean);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("oracle-{i}"));
        damage(volume.path(), at, blocksize, length, d);
        let judged = oracle("xfs_repair").arg("-n").arg(volume.path()).output();
        // A complaint from a tool that ignored the log is not a verdict
        // either, so it cannot stand as the corruption this needs.
        if repair::was_blind(&judged.repair_report()) {
            wrong.push(format!(
                "{}: xfs_repair -n ignored the log, so its report judges \
                 nothing:\n{}",
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
        damage(volume.path(), at, blocksize, length, d);
        if let Some(what) = the_write_went_ahead(volume.path()) {
            wrong.push(format!(
                "{}: xfs_repair -n reports corruption, and {what}",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
