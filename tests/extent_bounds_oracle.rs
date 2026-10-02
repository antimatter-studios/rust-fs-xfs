//! A write refuses an extent that does not lie inside one allocation group
//! (#92).
//!
//! A data fork's extents were decoded and used as they stood. Nothing
//! checked that an extent's group exists, that it starts past the group's
//! headers, or that it ends inside the group. An XFS block number is
//! packed, as `agno << agblklog | agbno`, and `agblklog` rounds the group
//! size up to a power of two. So a block number in
//! `[agblocks, 2^agblklog)` is no block of its own group: `fsblock_offset`
//! puts it at the start of the next group, where that group's superblock,
//! AGF, AGI and free list are. `write_at` wrote a file's bytes wherever the
//! extent said. `truncate_to_zero` gave the extent back to free space,
//! which freed another group's headers or wrote a free-space record past
//! the end of the group.
//!
//! The kernel refuses this. `xfs_bmap_validate_extent` runs as the fork is
//! read and requires `xfs_verify_fsbext`: the group exists, the start is
//! past `XFS_AGFL_BLOCK`, and the whole extent lies inside the group.
//!
//! The volume is a data fixture the kernel wrote. `/medium.bin` is one
//! 64-block extent, and the inode is damaged so that the extent points
//! somewhere else, with its checksum recomputed so only the extent is
//! wrong. `xfs_repair -n` judges that each damage is corruption. The
//! driver has to refuse both edits and ask the device for no write.

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
const SUITE: &str = "extent_bounds_oracle";

/// Built with `rmapbt=0`, so a free is not stopped by the reverse map
/// failing to match. That is the configuration in which the extent itself
/// is the only thing that can refuse it.
const FIXTURE: &str = "xfsdata-default.img";

/// One 64-block extent on every geometry the data fixtures build.
const FILE: &str = "/medium.bin";

/// Offsets in the v3 `xfs_dinode`.
const DI_CRC: usize = 100;
const DI_FORK: usize = 176;

/// Where the damaged extent points, from the volume's geometry.
struct Damage {
    what: &'static str,
    startblock: fn(&Geometry) -> u64,
    /// A new length, or `None` to keep the file's own.
    blockcount: Option<u64>,
}

struct Geometry {
    agblocks: u64,
    agcount: u64,
    agblklog: u32,
}

const DAMAGES: [Damage; 4] = [
    Damage {
        what: "the extent runs past the end of its group, into the next group's headers",
        startblock: |g| g.agblocks - 16,
        blockcount: None,
    },
    Damage {
        what: "the extent is in a group the filesystem does not have",
        startblock: |g| (g.agcount << g.agblklog) | 100,
        blockcount: None,
    },
    Damage {
        what: "the extent starts on its group's superblock",
        startblock: |g| 1 << g.agblklog,
        blockcount: None,
    },
    // HEADERS AND TREE ROOTS ONLY, none of them free. The longer extents
    // above also cover free space, so the free-space tree's overlap check
    // happens to stop a truncate that frees them. This one overlaps
    // nothing free, so only a check of the extent itself can.
    Damage {
        what: "the extent is exactly the next group's headers and tree roots",
        startblock: |g| 1 << g.agblklog,
        blockcount: Some(8),
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

fn mount_watched(image: &Path) -> (Filesystem, Arc<Watched>) {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged extent does not stop the mount");
    (fs, dev)
}

fn fresh_copy(tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &common::fixture(FIXTURE),
        &format!("{tag}-{}.img", std::process::id()),
    )
}

/// The file's inode number, where its record is, how big the record is and
/// the file's length, and the volume's geometry.
fn the_file(image: &Path) -> (u64, u64, usize, u64, Geometry) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    let inode = fs.lookup_path(FILE).expect("the fixture has the file");
    let (_, raw) = fs.read_inode_raw(inode.ino).expect("read its inode");
    assert_eq!(
        (inode.format, inode.nextents),
        (fs_xfs::inode::Format::Extents, 1),
        "{FILE} must be one extent held in the inode for this test to damage it"
    );
    assert_eq!(raw[4], 3, "the fixture is v5, so its inodes are v3");
    let sb = fs.superblock();
    (
        inode.ino,
        fs.inode_offset(inode.ino)
            .expect("the inode has an address"),
        usize::from(sb.inodesize),
        inode.size,
        Geometry {
            agblocks: u64::from(sb.agblocks),
            agcount: u64::from(sb.agcount),
            agblklog: u32::from(sb.agblklog),
        },
    )
}

/// Point the file's one extent at `startblock`, keeping its offset, and its
/// length unless `blockcount` gives one, and recompute the inode's
/// checksum.
fn damage(image: &Path, at: u64, isize: usize, startblock: u64, blockcount: Option<u64>) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut rec = vec![0u8; isize];
    f.read_exact(&mut rec).unwrap();

    // The packed record: flag:1 startoff:54 startblock:52 blockcount:21.
    let hi = u64::from_be_bytes(rec[DI_FORK..DI_FORK + 8].try_into().unwrap());
    let lo = u64::from_be_bytes(rec[DI_FORK + 8..DI_FORK + 16].try_into().unwrap());
    let flag_and_off = hi & !((1u64 << 9) - 1);
    let count = blockcount.unwrap_or(lo & ((1u64 << 21) - 1));
    let hi = flag_and_off | (startblock >> 43);
    let lo = (startblock << 21) | count;
    rec[DI_FORK..DI_FORK + 8].copy_from_slice(&hi.to_be_bytes());
    rec[DI_FORK + 8..DI_FORK + 16].copy_from_slice(&lo.to_be_bytes());

    rec[DI_CRC..DI_CRC + 4].copy_from_slice(&[0; 4]);
    let crc = crc32c::crc32c(&rec);
    rec[DI_CRC..DI_CRC + 4].copy_from_slice(&crc.to_le_bytes());

    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&rec).unwrap();
    f.sync_all().unwrap();
}

/// The two edits that act on a file's extents.
#[derive(Clone, Copy, Debug)]
enum Edit {
    /// Overwrite the whole file in place.
    Overwrite,
    /// Truncate it to zero, which gives its extents back to free space.
    TruncateToZero,
}

const EDITS: [Edit; 2] = [Edit::Overwrite, Edit::TruncateToZero];

/// Mount `image`, make `edit`, and say whether it went ahead: an `Ok`, or
/// any write asked of the device.
fn went_ahead(image: &Path, ino: u64, size: u64, edit: Edit) -> Option<String> {
    let (fs, dev) = mount_watched(image);
    let result = match edit {
        Edit::Overwrite => fs
            .read_inode_raw(ino)
            .and_then(|(inode, raw)| fs.write_at(&inode, &raw, 0, &vec![0xab; size as usize]))
            .map(|n| format!("{n} bytes")),
        Edit::TruncateToZero => fs.truncate_to_zero(ino).map(|lsn| format!("lsn {lsn}")),
    };
    let writes = dev.writes.load(Ordering::SeqCst);
    (result.is_ok() || writes != 0)
        .then(|| format!("{edit:?} returned {result:?} after {writes} writes"))
}

/// Each edit on its own fresh copy of the volume, damaged by `d`.
///
/// A fresh copy each, because an edit that wrongly goes ahead can leave a
/// volume the next mount refuses -- the overwrite lands on another group's
/// headers -- and the second edit would then be refused for that rather
/// than for the damage under test.
fn edits_on_damaged_copies(
    tag: &str,
    d: Option<&Damage>,
    file: &(u64, u64, usize, u64, Geometry),
) -> Vec<String> {
    let (ino, at, isize, size, geometry) = file;
    let mut out = Vec::new();
    for edit in EDITS {
        let volume = fresh_copy(&format!("{tag}-{edit:?}"));
        if let Some(d) = d {
            damage(
                volume.path(),
                *at,
                *isize,
                (d.startblock)(geometry),
                d.blockcount,
            );
        }
        if let Some(what) = went_ahead(volume.path(), *ino, *size, edit) {
            out.push(what);
        }
    }
    out
}

/// The driver refuses every damage and writes nothing.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn an_extent_outside_its_group_is_refused_before_anything_is_written() {
    let probe = fresh_copy("probe");
    let file = the_file(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let startblock = (d.startblock)(&file.4);
        for edit in edits_on_damaged_copies(&format!("driver-{i}"), Some(d), &file) {
            wrong.push(format!("{} (block {startblock}): {edit}", d.what));
        }
    }
    assert!(
        wrong.is_empty(),
        "an edit went ahead on an extent outside its group:\n  {}",
        wrong.join("\n  ")
    );

    // The undamaged file still takes both, so the refusals above are about
    // the damage and not about the volume.
    let went = edits_on_damaged_copies("driver-undamaged", None, &file);
    assert_eq!(
        went.len(),
        EDITS.len(),
        "the undamaged file takes the overwrite and the truncate: {went:?}"
    );
}

/// `xfs_repair -n` calls every damage corruption, and the driver refuses
/// each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_damaged_extent_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let file = the_file(clean.path());
    let judged = oracle("xfs_repair").args(["-n", &clean.guest()]).output();
    assert!(
        judged.ok(),
        "the undamaged copy must be clean to xfs_repair -n, or nothing below means \
         anything:\n{}",
        judged.repair_report()
    );
    drop(clean);

    let (_, at, isize, _, geometry) = &file;
    let mut wrong = Vec::new();
    for (i, d) in DAMAGES.iter().enumerate() {
        let startblock = (d.startblock)(geometry);
        let volume = fresh_copy(&format!("oracle-{i}"));
        damage(volume.path(), *at, *isize, startblock, d.blockcount);
        let judged = oracle("xfs_repair").args(["-n", &volume.guest()]).output();
        if judged.ok() {
            wrong.push(format!(
                "{} (block {startblock}): xfs_repair -n called it clean, so this is not \
                 a damage the driver should refuse:\n{}",
                d.what,
                judged.repair_report()
            ));
        }
        drop(volume);
        for edit in edits_on_damaged_copies(&format!("oracle-{i}"), Some(d), &file) {
            wrong.push(format!(
                "{} (block {startblock}): xfs_repair -n reports corruption, and {edit}",
                d.what
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
