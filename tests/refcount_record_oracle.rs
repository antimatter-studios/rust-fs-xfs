//! A free refuses a reference-count record the kernel would refuse (#332).
//!
//! `xfs_refcount_check_irec` judges each record of the reference-count
//! tree as it is read: it is not empty, a shared one has at least two
//! owners, and the run lies inside its group and past the group's
//! headers. The tree's key order adds that records ascend. A free asks
//! this tree which blocks another file still holds, and rewrites it in
//! the same transaction, so a record taken on trust would be edited on
//! the word of a tree the kernel calls corrupt (#92, #324).
//!
//! #324's tests build those records in memory. This takes a volume the
//! kernel made -- `xfsfeat-reflink.img`, where `/sf/shared.bin` and
//! `/sf/partial.bin` are `cp --reflink=always` copies of `/sf/data.bin`
//! -- damages one record of the group's refcount leaf, recomputes the
//! block's checksum so only the record is wrong, and requires both:
//!
//! - `xfs_repair -n` reports the volume damaged, and
//! - `truncate_to_zero` on a file whose extent is shared refuses, and
//!   asks the device for no write.
//!
//! The first is the reason for the second. A damage the checker called
//! clean would be a refusal of a sound volume, not a guard.

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
const SUITE: &str = "refcount_record_oracle";

/// Built with `reflink=1` and `rmapbt=0`, so a free is not stopped by the
/// reverse map failing to match: the refcount tree's records are the only
/// thing that can refuse it.
const FIXTURE: &str = "xfsfeat-reflink.img";

/// A file whose extent is shared, so freeing it consults the tree.
const FILE: &str = "/sf/data.bin";

/// Offsets in a v5 short-form B+tree block.
const BB_NUMRECS: usize = 6;
const BB_CRC: usize = 52;
const BB_RECORDS: usize = 56;
const RECORD: usize = 12;

/// One record as the tree holds it, and the geometry a damage needs.
#[derive(Clone, Copy)]
struct Rec {
    startblock: u32,
    blockcount: u32,
    refcount: u32,
}

struct Leaf {
    /// Byte offset of the leaf on the device.
    at: u64,
    blocksize: usize,
    records: Vec<Rec>,
    /// The group's length in blocks.
    length: u32,
}

/// One damage: what the tree holds after it, and why that is wrong.
struct Damage {
    what: &'static str,
    apply: fn(&mut Vec<Rec>, u32),
}

const DAMAGES: [Damage; 5] = [
    Damage {
        what: "an empty record",
        apply: |r, _| r[0].blockcount = 0,
    },
    Damage {
        what: "a shared record with one owner",
        apply: |r, _| r[0].refcount = 1,
    },
    Damage {
        what: "a record on the group's headers",
        apply: |r, _| {
            r[0].startblock = 0;
            r[0].blockcount = 1;
        },
    },
    Damage {
        what: "a record past the end of its group",
        apply: |r, length| {
            let last = r.len() - 1;
            r[last].startblock = length - 1;
            r[last].blockcount = 8;
        },
    },
    Damage {
        what: "two records out of order",
        apply: |r, _| r.swap(0, 1),
    },
];

/// A writable device that counts what it is asked to write, so a refusal
/// can be shown to have written nothing at all.
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

/// Where group 0's refcount leaf is and what it holds, read from the
/// undamaged volume.
fn the_leaf(image: &Path) -> Leaf {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    let sb = fs.superblock();
    assert!(sb.is_v5(), "the checksum recomputed below is the v5 one");
    let agf = fs.agf(0).expect("the group's header");
    assert_eq!(
        agf.refcount_level, 1,
        "the refcount tree must be one leaf for this test to damage a record in it"
    );
    let records: Vec<Rec> = fs
        .refcount_records(0)
        .expect("the refcount records")
        .iter()
        .map(|r| Rec {
            startblock: r.startblock,
            blockcount: r.blockcount,
            refcount: r.refcount,
        })
        .collect();
    assert!(
        records.len() >= 2 && records.iter().all(|r| r.refcount >= 2),
        "{FIXTURE} must hold at least two shared records, so that two can be put out of \
         order: {:?}",
        records
            .iter()
            .map(|r| (r.startblock, r.blockcount, r.refcount))
            .collect::<Vec<_>>()
    );
    Leaf {
        at: sb.fsblock_offset(u64::from(agf.refcount_root)),
        blocksize: sb.blocksize as usize,
        records,
        length: agf.length,
    }
}

/// Write `records` over the leaf's own, and recompute the block's
/// checksum so that only the records are wrong.
fn damage(image: &Path, leaf: &Leaf, records: &[Rec]) {
    assert_eq!(
        records.len(),
        leaf.records.len(),
        "a damage changes records, not how many there are"
    );
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(leaf.at)).unwrap();
    let mut block = vec![0u8; leaf.blocksize];
    f.read_exact(&mut block).unwrap();
    let numrecs = u16::from_be_bytes(block[BB_NUMRECS..BB_NUMRECS + 2].try_into().unwrap());
    assert_eq!(
        usize::from(numrecs),
        records.len(),
        "the leaf holds the records the driver read"
    );
    for (i, r) in records.iter().enumerate() {
        let at = BB_RECORDS + i * RECORD;
        block[at..at + 4].copy_from_slice(&r.startblock.to_be_bytes());
        block[at + 4..at + 8].copy_from_slice(&r.blockcount.to_be_bytes());
        block[at + 8..at + 12].copy_from_slice(&r.refcount.to_be_bytes());
    }
    block[BB_CRC..BB_CRC + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c::crc32c(&block);
    block[BB_CRC..BB_CRC + 4].copy_from_slice(&crc.to_le_bytes());
    f.seek(SeekFrom::Start(leaf.at)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
}

/// Truncate the file to zero on `image`, and say whether it went ahead: an
/// `Ok`, or any write asked of the device. A mount that refuses the
/// volume is a refusal too, and writes nothing.
fn truncate_went_ahead(image: &Path) -> Option<String> {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the volume"),
        writes: AtomicUsize::new(0),
    });
    let Ok(fs) = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>) else {
        return (dev.writes.load(Ordering::SeqCst) != 0)
            .then(|| "the mount wrote to the device".to_string());
    };
    let ino = fs.lookup_path(FILE).expect("the fixture has the file").ino;
    let result = fs.truncate_to_zero(ino);
    let writes = dev.writes.load(Ordering::SeqCst);
    (result.is_ok() || writes != 0)
        .then(|| format!("truncate_to_zero returned {result:?} after {writes} writes"))
}

/// The driver refuses to free on every damaged record, and still frees on
/// the undamaged volume, so the refusals are about the damage.
#[test]
fn a_damaged_refcount_record_is_refused_before_anything_is_written() {
    let probe = fresh_copy("probe");
    let leaf = the_leaf(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("driver-{i}"));
        let mut records = leaf.records.clone();
        (d.apply)(&mut records, leaf.length);
        damage(volume.path(), &leaf, &records);
        if let Some(what) = truncate_went_ahead(volume.path()) {
            wrong.push(format!("{}: {what}", d.what));
        }
    }
    assert!(
        wrong.is_empty(),
        "a free went ahead on a damaged refcount record:\n  {}",
        wrong.join("\n  ")
    );

    let clean = fresh_copy("driver-undamaged");
    assert!(
        truncate_went_ahead(clean.path()).is_some(),
        "the undamaged volume takes the truncate, so the refusals above are about the damage"
    );
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
#[test]
fn every_damaged_refcount_record_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let leaf = the_leaf(clean.path());
    // The host path, which is inside the repository and so is the same
    // path in the guest. Graded by repair::assert_agreed rather than the
    // exit status alone (#124).
    let judged = oracle("xfs_repair").arg("-n").arg(clean.path()).output();
    repair::assert_agreed(
        &judged.repair_report(),
        "the undamaged copy must be clean to xfs_repair -n, or nothing below means anything",
    );
    drop(clean);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("oracle-{i}"));
        let mut records = leaf.records.clone();
        (d.apply)(&mut records, leaf.length);
        damage(volume.path(), &leaf, &records);
        let judged = oracle("xfs_repair").arg("-n").arg(volume.path()).output();
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
        if let Some(what) = truncate_went_ahead(volume.path()) {
            wrong.push(format!(
                "{}: xfs_repair -n reports corruption, and {what}",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
