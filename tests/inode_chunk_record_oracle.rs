//! A create refuses an inode-chunk record whose free count disagrees with
//! its free mask (#314).
//!
//! The inode tree's blocks are verified on every path that writes: magic,
//! CRC, UUID, owner, address and level. The records inside them were used
//! as they stood, with two consequences:
//!
//! - A create picks a free inode by the record's free mask, then does
//!   `freecount -= 1` on the record's `u8` free count. A count above the
//!   mask's is carried into the rewritten record, so both inode trees
//!   claim free inodes that do not exist. A count of zero beside free bits
//!   underflows: a panic in a debug build, and 255 written back in release.
//! - `Trees::open` turned a record `record()` refused into an all-zero
//!   chunk and carried on. Laying the trees out again then wrote that chunk
//!   in place of the one it could not read.
//!
//! The kernel refuses both. `xfs_inobt_check_irec` requires
//! `ir_freecount == popcount(ir_free)` over the inodes the chunk has, a
//! count between 4 and 64, and a start inside the group.
//!
//! The volume is a fresh `mkfs.xfs` fixture. The root's chunk record in
//! group 0's inode tree is damaged, with the block's checksum recomputed so
//! only the record is wrong. `xfs_repair -n` judges that each damage is
//! corruption. A create has to be refused before it asks the device for
//! anything, and must not panic.

mod common;

use common::{oracle, scratch};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "inode_chunk_record_oracle";

/// A fresh volume, so group 0's inode tree is one leaf holding the root's
/// chunk, with free inodes left in it.
const FIXTURE: &str = "xfs-default.img";

/// The v5 short-form B+tree block header: the CRC's offset and the length.
const BT_CRC: usize = 52;
const BT_HEADER: usize = 56;

/// The sparse record's free count: startino (4), holemask (2), count (1),
/// freecount (1), free (8). `xfs-default.img` has sparse inodes, as
/// `mkfs.xfs` makes by default.
const REC_FREECOUNT: usize = 7;

/// One damage to the first record's free count, given the true count.
struct Damage {
    what: &'static str,
    freecount: fn(u8) -> u8,
}

const DAMAGES: [Damage; 2] = [
    Damage {
        what: "the free count is two above the free mask's",
        freecount: |n| n + 2,
    },
    Damage {
        what: "the free count is zero beside a mask with free inodes in it",
        freecount: |_| 0,
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

/// Where group 0's inode-tree leaf is, and the block size.
fn the_leaf(image: &Path) -> (u64, usize) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    assert!(
        fs.superblock().has_sparse_inodes(),
        "{FIXTURE} must have sparse inodes, whose record this test damages"
    );
    let agi = fs.read_agi(0).expect("group 0's AGI");
    assert_eq!(
        agi.level, 1,
        "group 0's inode tree must be one leaf for this test to damage it"
    );
    let blocksize = fs.superblock().blocksize;
    (
        u64::from(agi.root) * u64::from(blocksize),
        blocksize as usize,
    )
}

/// Rewrite the first record's free count as `d` says, and recompute the
/// block's checksum.
fn damage(image: &Path, at: u64, blocksize: usize, d: &Damage) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut block = vec![0u8; blocksize];
    f.read_exact(&mut block).unwrap();

    let at_count = BT_HEADER + REC_FREECOUNT;
    block[at_count] = (d.freecount)(block[at_count]);

    block[BT_CRC..BT_CRC + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c::crc32c(&block);
    block[BT_CRC..BT_CRC + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&block).unwrap();
    f.sync_all().unwrap();
}

/// Create a file, and say whether the create went ahead: an `Ok`, any
/// device write, or a panic.
fn the_create_went_ahead(image: &Path) -> Option<String> {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged inode-chunk record does not stop the mount");
    let root = fs.superblock().rootino;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fs.create_file(root, b"new", 0o100644)
    }));
    let writes = dev.writes.load(Ordering::SeqCst);
    match result {
        Err(_) => Some(format!("create_file panicked after {writes} writes")),
        Ok(r) => (r.is_ok() || writes != 0)
            .then(|| format!("create_file returned {r:?} after {writes} writes")),
    }
}

/// The driver refuses every damage, writes nothing and does not panic.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn a_create_refuses_a_chunk_whose_free_count_disagrees_with_its_mask() {
    let probe = fresh_copy("probe");
    let (at, blocksize) = the_leaf(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("driver-{i}"));
        damage(volume.path(), at, blocksize, d);
        if let Some(what) = the_create_went_ahead(volume.path()) {
            wrong.push(format!("{}: {what}", d.what));
        }
    }
    assert!(
        wrong.is_empty(),
        "a create went ahead on a chunk record whose free count is wrong:\n  {}",
        wrong.join("\n  ")
    );

    // The undamaged record still takes the create, so the refusals above
    // are about the damage and not about the volume.
    let volume = fresh_copy("driver-undamaged");
    assert!(
        the_create_went_ahead(volume.path()).is_some(),
        "the undamaged volume takes the create"
    );
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_damaged_chunk_record_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let (at, blocksize) = the_leaf(clean.path());
    let judged = oracle("xfs_repair").args(["-n", &clean.guest()]).output();
    assert!(
        judged.ok(),
        "the undamaged copy must be clean to xfs_repair -n, or nothing below means \
         anything:\n{}",
        judged.repair_report()
    );
    drop(clean);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("oracle-{i}"));
        damage(volume.path(), at, blocksize, d);
        let judged = oracle("xfs_repair").args(["-n", &volume.guest()]).output();
        if judged.ok() {
            wrong.push(format!(
                "{}: xfs_repair -n called it clean, so this is not a damage the driver \
                 should refuse:\n{}",
                d.what,
                judged.repair_report()
            ));
        }
        drop(volume);
        let volume = fresh_copy(&format!("oracle-{i}-driver"));
        damage(volume.path(), at, blocksize, d);
        if let Some(what) = the_create_went_ahead(volume.path()) {
            wrong.push(format!(
                "{}: xfs_repair -n reports corruption, and {what}",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
