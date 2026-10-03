//! An inode whose version the filesystem does not use is refused (#92).
//!
//! `Inode::parse` accepted versions 1, 2 and 3 on any filesystem, and
//! checked the CRC, `di_ino` and `di_uuid` only when the record itself said
//! it was version 3. So on a v5 filesystem one flipped bit in the version
//! byte -- 3 is `0b11`, 2 is `0b10` -- turned every one of those checks
//! off. The same byte also moves the data fork: a v2 core is 100 bytes and
//! a v3 core 176, so the fork was read from the middle of the v3 core's
//! checksum, LSN and UUID. A write path then acted on extents decoded from
//! those bytes.
//!
//! The kernel refuses this before it reads anything else.
//! `xfs_dinode_good_version` requires version 3 on a filesystem with v3
//! inodes and 1 or 2 on one without, and the inode verifier rejects any
//! other version.
//!
//! The volume is a data fixture the kernel wrote. `/medium.bin`'s version
//! byte is changed and nothing else. `xfs_repair -n` judges that the result
//! is corruption. The driver has to refuse to read the inode, and refuse
//! the in-place overwrite the C ABI makes from that read, writing nothing.

mod common;

use common::{oracle, repair, scratch};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "inode_version_oracle";

/// A v5 data fixture, so its inodes are version 3.
const FIXTURE: &str = "xfsdata-default.img";

const FILE: &str = "/medium.bin";

/// `di_version`, in every inode core.
const DI_VERSION: u64 = 4;

/// The versions written over the file's 3: one bit away, and two.
const WRONG_VERSIONS: [u8; 2] = [2, 1];

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

/// The file's inode number, where its record is, and its length.
fn the_file(image: &Path) -> (u64, u64, u64) {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image).expect("open")))
        .expect("mount the undamaged volume");
    assert!(fs.superblock().is_v5(), "{FIXTURE} must be a v5 volume");
    let inode = fs.lookup_path(FILE).expect("the fixture has the file");
    assert_eq!(inode.version, 3, "a v5 volume's inodes are version 3");
    (
        inode.ino,
        fs.inode_offset(inode.ino)
            .expect("the inode has an address"),
        inode.size,
    )
}

fn set_version(image: &Path, at: u64, version: u8) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(image)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at + DI_VERSION)).unwrap();
    f.write_all(&[version]).unwrap();
    f.sync_all().unwrap();
}

/// What the driver accepted of the damaged inode: the read, and the
/// in-place overwrite `fs_xfs_write_file` makes from that read.
fn accepted(image: &Path, ino: u64, size: u64) -> Vec<String> {
    let dev = Arc::new(Watched {
        inner: FileDevice::open_rw(image).expect("open the damaged volume"),
        writes: AtomicUsize::new(0),
    });
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>)
        .expect("a damaged file inode does not stop the mount");
    let mut out = Vec::new();
    match fs.read_inode_raw(ino) {
        Ok((inode, raw)) => {
            out.push(format!(
                "read_inode_raw accepted it as version {} with format {:?} and {} extents",
                inode.version, inode.format, inode.nextents
            ));
            let wrote = fs.write_at(&inode, &raw, 0, &vec![0xab; size as usize]);
            let writes = dev.writes.load(Ordering::SeqCst);
            if wrote.is_ok() || writes != 0 {
                out.push(format!("write_at returned {wrote:?} after {writes} writes"));
            }
        }
        Err(_) => {
            let writes = dev.writes.load(Ordering::SeqCst);
            if writes != 0 {
                out.push(format!("the refused read was followed by {writes} writes"));
            }
        }
    }
    out
}

/// The driver refuses the read and writes nothing.
///
/// Needs only the fixture, so it is the red and the green a host without
/// the VM can run.
#[test]
fn an_inode_version_the_filesystem_does_not_use_is_refused() {
    let probe = fresh_copy("probe");
    let (ino, at, size) = the_file(probe.path());
    drop(probe);

    let mut wrong = Vec::new();
    for version in WRONG_VERSIONS {
        let volume = fresh_copy(&format!("driver-v{version}"));
        set_version(volume.path(), at, version);
        for what in accepted(volume.path(), ino, size) {
            wrong.push(format!(
                "version {version} on a v5 volume (inode {ino}): {what}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "an inode whose version the filesystem does not use was acted on:\n  {}",
        wrong.join("\n  ")
    );

    // The undamaged inode still reads, so the refusals above are about the
    // version and not about the volume.
    let volume = fresh_copy("driver-undamaged");
    let fs =
        Filesystem::mount(Arc::new(FileDevice::open(volume.path()).expect("open"))).expect("mount");
    fs.read_inode_raw(ino).expect("the undamaged inode reads");
}

/// `xfs_repair -n` calls each wrong version corruption, and the driver
/// refuses each one.
///
/// The judge is the reason this test exists beside the one above: a damage
/// the checker accepted would be a refusal of a sound volume, not a guard.
#[test]
fn every_wrong_version_is_corruption_to_xfs_repair_and_refused_here() {
    let clean = fresh_copy("oracle-undamaged");
    let (ino, at, size) = the_file(clean.path());
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
    for version in WRONG_VERSIONS {
        let volume = fresh_copy(&format!("oracle-v{version}"));
        set_version(volume.path(), at, version);
        let judged = oracle("xfs_repair").arg("-n").arg(volume.path()).output();
        // A complaint from a tool that ignored the log is not a verdict
        // either, so it cannot stand as the corruption this needs.
        if repair::was_blind(&judged.repair_report()) {
            wrong.push(format!(
                "version {version} (inode {ino}): xfs_repair -n ignored the log, so its report judges \
                 nothing:\n{}",
                judged.repair_report()
            ));
        } else if judged.ok() {
            wrong.push(format!(
                "version {version} (inode {ino}): xfs_repair -n called it clean, so this \
                 is not a damage the driver should refuse:\n{}",
                judged.repair_report()
            ));
        }
        for what in accepted(volume.path(), ino, size) {
            wrong.push(format!(
                "version {version} (inode {ino}): xfs_repair -n reports corruption, and {what}"
            ));
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}
