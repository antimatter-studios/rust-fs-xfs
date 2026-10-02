//! A create takes only an inode slot that is verifiably free (#92).
//!
//! A create reads the free inode's slot and changes what a file needs
//! changed, keeping the magic, version, `di_ino` and `di_uuid` already
//! there. It read that slot straight off the device and checked none of
//! it: not the checksum, not the identity fields, and not that the slot
//! is free at all. So the inode tree's word that an inode was free was
//! the only thing standing between a create and a live file. A stale or
//! misread free bit handed out an inode still in use, and the create
//! journalled a new file over it. The old file's extents stayed allocated
//! to an inode that no longer described them, and recovery stamped a fresh
//! checksum on the result.
//!
//! The kernel refuses this. `xfs_iget` with `XFS_IGET_CREATE` reports
//! "Corruption detected! Free inode ... not marked free on disk" and
//! returns `EFSCORRUPTED`; the inode's own verifier checks the checksum,
//! `di_ino` and `di_uuid` before that.
//!
//! The volume is a fixture `mkfs.xfs` built. The test asks the driver which
//! inode a create takes, then damages that one slot at a time. Anything
//! but the checksum is damaged with the checksum recomputed, so only the
//! named field is wrong. `xfs_repair -n` is the judge that each damage is
//! corruption. The driver has to refuse the create and write nothing.

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
const SUITE: &str = "create_free_slot_oracle";

/// The fixture: a fresh `mkfs.xfs` volume with the default features, so
/// the inode a create takes is a free slot in the root's chunk.
const FIXTURE: &str = "xfs-default.img";

/// Offsets in the v3 `xfs_dinode`.
mod dinode {
    pub const MAGIC: usize = 0;
    pub const MODE: usize = 2;
    pub const FORMAT: usize = 5;
    pub const NLINK: usize = 16;
    pub const AFORMAT: usize = 83;
    pub const GEN: usize = 92;
    pub const CRC: usize = 100;
    pub const INO: usize = 152;
    pub const UUID: usize = 160;
}

/// One damage to the free slot.
struct Damage {
    what: &'static str,
    apply: fn(&mut [u8]),
    /// Leave the checksum wrong rather than recomputing it.
    break_crc: bool,
}

const DAMAGES: [Damage; 5] = [
    Damage {
        what: "the slot holds a live regular file the inode tree calls free",
        apply: |s| {
            s[dinode::MODE..dinode::MODE + 2].copy_from_slice(&0o100644u16.to_be_bytes());
            s[dinode::FORMAT] = 2; // extents
            s[dinode::NLINK..dinode::NLINK + 4].copy_from_slice(&1u32.to_be_bytes());
            s[dinode::AFORMAT] = 2;
        },
        break_crc: false,
    },
    Damage {
        what: "the slot's checksum is wrong",
        apply: |s| s[dinode::GEN] ^= 0x5a,
        break_crc: true,
    },
    Damage {
        what: "the slot names another inode in di_ino",
        apply: |s| s[dinode::INO + 7] ^= 0x01,
        break_crc: false,
    },
    Damage {
        what: "the slot carries another filesystem's UUID",
        apply: |s| s[dinode::UUID] ^= 0xff,
        break_crc: false,
    },
    Damage {
        what: "the slot's magic is not an inode's",
        apply: |s| s[dinode::MAGIC] ^= 0xff,
        break_crc: false,
    },
];

/// A writable device that counts what it is asked to write, so a refusal
/// can be shown to have written nothing at all -- not the log record, not
/// a pad, not a byte of the slot.
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

fn read_bytes(path: &Path, at: u64, len: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open the volume");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

fn write_bytes(path: &Path, at: u64, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

/// The slot a create takes on this volume, and where it is.
///
/// Asked of the driver on a throwaway copy rather than worked out here,
/// so the test damages the inode the create really reads.
fn the_slot_a_create_takes() -> (u64, u64, usize) {
    let probe = fresh_copy("probe");
    let dev = Arc::new(FileDevice::open_rw(probe.path()).expect("open the probe copy"));
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount the probe copy");
    let root = fs.superblock().rootino;
    let (ino, _) = fs
        .create_file(root, b"probe", 0o100644)
        .expect("a create on the undamaged volume succeeds");
    let at = fs.inode_offset(ino).expect("the new inode has an address");
    (ino, at, usize::from(fs.superblock().inodesize))
}

/// A copy of the fixture to damage, removed when it is dropped.
fn fresh_copy(tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &common::fixture(FIXTURE),
        &format!("{tag}-{}.img", std::process::id()),
    )
}

/// Damage the slot on `image` as `d` says.
fn damage(image: &Path, at: u64, isize: usize, d: &Damage) {
    let mut slot = read_bytes(image, at, isize);
    (d.apply)(&mut slot);
    if !d.break_crc {
        slot[dinode::CRC..dinode::CRC + 4].copy_from_slice(&[0; 4]);
        let crc = crc32c::crc32c(&slot);
        slot[dinode::CRC..dinode::CRC + 4].copy_from_slice(&crc.to_le_bytes());
    }
    write_bytes(image, at, &slot);
}

/// Mount `image` read-write, try the create, and say what happened and how
/// many writes it asked for.
fn try_create(image: &Path) -> (Result<(u64, u64), fs_xfs::Error>, usize) {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the damaged volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged free inode does not stop the mount");
    let root = fs.superblock().rootino;
    let result = fs.create_file(root, b"new", 0o100644);
    drop(fs);
    (result, dev.writes.load(Ordering::SeqCst))
}

/// The driver refuses every damage and writes nothing.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn a_create_refuses_a_slot_that_is_not_a_verified_free_inode() {
    let (ino, at, isize) = the_slot_a_create_takes();

    // A FRESH COPY FOR EACH DAMAGE. A create that is wrongly accepted
    // writes a log record, and the next mount would then refuse the
    // volume for its dirty log rather than for the damage under test.
    let mut accepted = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let volume = fresh_copy(&format!("driver-{i}"));
        damage(volume.path(), at, isize, d);
        let (result, writes) = try_create(volume.path());
        if result.is_ok() || writes != 0 {
            accepted.push(format!(
                "{} (inode {ino}): create returned {result:?} after {writes} writes",
                d.what
            ));
        }
    }
    assert!(
        accepted.is_empty(),
        "a create went ahead on a slot that is not a verified free inode:\n  {}",
        accepted.join("\n  ")
    );

    // The undamaged slot still takes the create, so the refusals above are
    // about the damage and not about the volume.
    let volume = fresh_copy("driver-undamaged");
    let (result, _) = try_create(volume.path());
    let (made, _) = result.expect("the undamaged slot takes the create");
    assert_eq!(made, ino, "the create took the slot this test damaged");
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_damaged_slot_is_corruption_to_xfs_repair_and_refused_here() {
    let (ino, at, isize) = the_slot_a_create_takes();

    let clean = fresh_copy("oracle-undamaged");
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
        damage(volume.path(), at, isize, d);
        let judged = oracle("xfs_repair").args(["-n", &volume.guest()]).output();
        if judged.ok() {
            wrong.push(format!(
                "{} (inode {ino}): xfs_repair -n called it clean, so this is not a \
                 damage the driver should refuse:\n{}",
                d.what,
                judged.repair_report()
            ));
        }
        let (result, writes) = try_create(volume.path());
        if result.is_ok() || writes != 0 {
            wrong.push(format!(
                "{} (inode {ino}): xfs_repair -n reports corruption, and create returned \
                 {result:?} after {writes} writes",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
