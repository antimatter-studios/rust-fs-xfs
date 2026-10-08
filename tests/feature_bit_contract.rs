//! Feature-mask refusals over an independently formatted fixture.
//! Only the superblock sector is overlaid; the source image stays read-only.

mod common;

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::superblock::{incompat, offsets, ro_compat};
use fs_xfs::{Error, Filesystem};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Probe {
    source: FileDevice,
    header: Vec<u8>,
    writes: AtomicUsize,
    inode_overlay: Option<(u64, Vec<u8>)>,
}

impl Probe {
    fn with_bit(field: usize, bit: u32) -> Arc<Self> {
        Self::with_overlay(|header| {
            let existing = u32::from_be_bytes(header[field..field + 4].try_into().unwrap());
            header[field..field + 4].copy_from_slice(&(existing | bit).to_be_bytes());
        })
    }

    fn with_overlay(change: impl FnOnce(&mut [u8])) -> Arc<Self> {
        let source = FileDevice::open(common::fixture("xfsfeat-base.img")).unwrap();
        let mut header = vec![0; 4096];
        source.read_at(0, &mut header).unwrap();
        change(&mut header);
        let sector = usize::from(u16::from_be_bytes(
            header[offsets::SECTSIZE..offsets::SECTSIZE + 2]
                .try_into()
                .unwrap(),
        ));
        fs_xfs::super_write::stamp_crc(&mut header[..sector]);
        Arc::new(Self {
            source,
            header,
            writes: AtomicUsize::new(0),
            inode_overlay: None,
        })
    }
}

#[test]
fn active_quota_accounting_is_readable_but_refuses_all_write_mounts() {
    // Preserve the regression name; recognized accounting is now supported.
    // Each accounting type combined with an unknown flag remains read-only.
    for bit in [1u16, 1 << 3, 1 << 6] {
        let device = Probe::with_overlay(|header| {
            header[offsets::QFLAGS..offsets::QFLAGS + 2]
                .copy_from_slice(&(bit | 0x8000).to_be_bytes());
        });
        Filesystem::mount(device.clone()).unwrap();
        assert!(
            matches!(
                Filesystem::mount_rw(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "quota flags {bit:#x} accepted for mutation"
        );
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn legacy_unknown_bits_refuse_both_mounts_before_mutation() {
    for field in [offsets::FEATURES2, offsets::BAD_FEATURES2] {
        for index in 0..32 {
            let bit = 1u32 << index;
            if bit & 0x38a != 0 {
                continue;
            }
            let device = Probe::with_bit(field, bit);
            assert!(matches!(
                Filesystem::mount(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ));
            assert!(matches!(
                Filesystem::mount_rw(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ));
            assert_eq!(device.writes.load(Ordering::SeqCst), 0);
        }
    }
    let device = Probe::with_overlay(|header| {
        let version = u16::from_be_bytes(header[100..102].try_into().unwrap());
        header[100..102].copy_from_slice(&(version | 0x0200).to_be_bytes());
    });
    assert!(matches!(
        Filesystem::mount(device.clone()),
        Err(Error::UnsupportedFeature(_))
    ));
    assert!(matches!(
        Filesystem::mount_rw(device.clone()),
        Err(Error::UnsupportedFeature(_))
    ));
    assert_eq!(device.writes.load(Ordering::SeqCst), 0);
}

impl BlockRead for Probe {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.source.read_at(offset, buf)?;
        if offset < self.header.len() as u64 {
            let start = offset as usize;
            let count = buf.len().min(self.header.len() - start);
            buf[..count].copy_from_slice(&self.header[start..start + count]);
        }
        if let Some((at, bytes)) = &self.inode_overlay {
            let start = offset.max(*at);
            let end = (offset + buf.len() as u64).min(*at + bytes.len() as u64);
            if start < end {
                buf[(start - offset) as usize..(end - offset) as usize]
                    .copy_from_slice(&bytes[(start - at) as usize..(end - at) as usize]);
            }
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.source.size_bytes()
    }
}

impl BlockDevice for Probe {
    fn write_at(&self, _: u64, _: &[u8]) -> fs_core::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        panic!("feature refusal attempted to mutate the device")
    }
    fn is_writable(&self) -> bool {
        true
    }
}

#[test]
fn source_fixture_reads_and_checks_without_mutation() {
    let device = Probe::with_bit(offsets::FEATURES_COMPAT, 0);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let file = fs.lookup_path("/sf/data.bin").unwrap();
    assert_eq!(file.size, 32 * 4096);
    let report = fs_xfs::check::check(&fs);
    assert!(!report.dirty, "source fixture has an unreplayed log");
    assert!(report.inodes > 0);
    assert!(report.directories > 0);
    assert!(report.is_clean(), "fixture check: {:?}", report.findings);
    assert_eq!(device.writes.load(Ordering::SeqCst), 0);
}

#[test]
fn every_unsupported_incompat_bit_refuses_before_mutation() {
    for index in 0..32 {
        let bit = 1u32 << index;
        if bit & incompat::SUPPORTED != 0 {
            continue;
        }
        let device = Probe::with_bit(offsets::FEATURES_INCOMPAT, bit);
        assert!(
            matches!(
                Filesystem::mount(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "read accepted incompat {bit:#x}"
        );
        assert!(
            matches!(
                Filesystem::mount_rw(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "write accepted incompat {bit:#x}"
        );
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn every_unknown_ro_compat_bit_is_readable_but_refuses_write_before_mutation() {
    for index in 0..32 {
        let bit = 1u32 << index;
        if bit & ro_compat::SUPPORTED != 0 {
            continue;
        }
        let device = Probe::with_bit(offsets::FEATURES_RO_COMPAT, bit);
        Filesystem::mount(device.clone())
            .unwrap_or_else(|e| panic!("read refused ro_compat {bit:#x}: {e}"));
        assert!(
            matches!(
                Filesystem::mount_rw(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "write accepted ro_compat {bit:#x}"
        );
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn readable_unmaintained_features_refuse_write_before_mutation() {
    for bit in [incompat::PARENT, incompat::EXCHRANGE, incompat::READ_ONLY] {
        let device = Probe::with_bit(offsets::FEATURES_INCOMPAT, bit);
        Filesystem::mount(device.clone()).unwrap();
        assert!(matches!(
            Filesystem::mount_rw(device.clone()),
            Err(Error::UnsupportedFeature(_))
        ));
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn every_unsupported_log_incompat_bit_refuses_before_mutation() {
    for index in 0..32 {
        let bit = 1u32 << index;
        let device = Probe::with_bit(offsets::FEATURES_LOG_INCOMPAT, bit);
        assert!(
            matches!(
                Filesystem::mount(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "read accepted log incompat {bit:#x}"
        );
        assert!(
            matches!(
                Filesystem::mount_rw(device.clone()),
                Err(Error::UnsupportedFeature(_))
            ),
            "write accepted log incompat {bit:#x}"
        );
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn every_compatible_bit_is_retained_without_mutation() {
    for index in 0..32 {
        let bit = 1u32 << index;
        let device = Probe::with_bit(offsets::FEATURES_COMPAT, bit);
        let read = Filesystem::mount(device.clone()).unwrap();
        assert_eq!(read.superblock().features_compat & bit, bit);
        let write = Filesystem::mount_rw(device.clone()).unwrap();
        assert_eq!(write.superblock().features_compat & bit, bit);
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

fn unsupported<T>(result: fs_xfs::Result<T>, expected: &str) {
    match result {
        Err(Error::UnsupportedFeature(actual)) => assert_eq!(actual, expected),
        Err(other) => panic!("expected unsupported {expected:?}, got {other}"),
        Ok(_) => panic!("accepted unsupported {expected:?}"),
    }
}

fn read_populated_file(fs: &Filesystem) {
    assert_eq!(fs.read_path("/sf/data.bin").unwrap().len(), 32 * 4096);
}

fn fixture_digest() -> Vec<u8> {
    let mut file = std::fs::File::open(common::fixture("xfsfeat-base.img")).unwrap();
    let mut hash = Sha256::new();
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            return hash.finalize().to_vec();
        }
        hash.update(&buffer[..count]);
    }
}

#[test]
fn gate_errors_are_exact_and_accepted_mounts_read_without_changing_the_fixture() {
    let before = fixture_digest();
    for index in 0..32 {
        let bit = 1u32 << index;
        if bit & incompat::SUPPORTED == 0 {
            let expected = match bit {
                incompat::NEEDSREPAIR => "this volume is marked as needing repair (incompat needsrepair): run xfs_repair on it before mounting".to_string(),
                incompat::METADIR => "incompatible features not implemented: the metadata directory tree (metadir)".to_string(),
                _ => format!("incompatible features not implemented: unknown bits {bit:#010x}"),
            };
            let device = Probe::with_bit(offsets::FEATURES_INCOMPAT, bit);
            unsupported(Filesystem::mount(device.clone()), &expected);
            unsupported(Filesystem::mount_rw(device.clone()), &expected);
            assert_eq!(device.writes.load(Ordering::SeqCst), 0);
        }
        let device = Probe::with_bit(offsets::FEATURES_LOG_INCOMPAT, bit);
        let expected = format!("log-incompatible features not implemented: {bit:#010x}");
        unsupported(Filesystem::mount(device.clone()), &expected);
        unsupported(Filesystem::mount_rw(device.clone()), &expected);
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
        if bit & ro_compat::SUPPORTED == 0 {
            let device = Probe::with_bit(offsets::FEATURES_RO_COMPAT, bit);
            read_populated_file(&Filesystem::mount(device.clone()).unwrap());
            unsupported(Filesystem::mount_rw(device.clone()), &format!("this volume sets read-only-compatible feature bits {bit:#x} that this driver does not maintain, so it can be read but not written"));
            assert_eq!(device.writes.load(Ordering::SeqCst), 0);
        }
        let device = Probe::with_bit(offsets::FEATURES_COMPAT, bit);
        read_populated_file(&Filesystem::mount(device.clone()).unwrap());
        read_populated_file(&Filesystem::mount_rw(device.clone()).unwrap());
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
        if bit & 0x38a == 0 {
            for field in [offsets::FEATURES2, offsets::BAD_FEATURES2] {
                let device = Probe::with_bit(field, bit);
                let expected = format!("unknown legacy features2 bits {bit:#010x}");
                unsupported(Filesystem::mount(device.clone()), &expected);
                unsupported(Filesystem::mount_rw(device.clone()), &expected);
                assert_eq!(device.writes.load(Ordering::SeqCst), 0);
            }
        }
    }
    for (bits, named) in [
        (incompat::PARENT, "parent pointers (parent)"),
        (incompat::EXCHRANGE, "exchange-range (exchrange)"),
        (
            incompat::READ_ONLY,
            "parent pointers (parent) and exchange-range (exchrange)",
        ),
    ] {
        let device = Probe::with_bit(offsets::FEATURES_INCOMPAT, bits);
        read_populated_file(&Filesystem::mount(device.clone()).unwrap());
        unsupported(Filesystem::mount_rw(device.clone()), &format!("this volume uses {named}, which this driver does not maintain, so it can be read but not written"));
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
    assert_eq!(fixture_digest(), before);
}

#[test]
fn supported_quota_flags_allow_write_mounts_and_unknown_flags_refuse() {
    for index in 0..16 {
        let bit = 1u16 << index;
        let device = Probe::with_overlay(|header| {
            header[offsets::QFLAGS..offsets::QFLAGS + 2].copy_from_slice(&bit.to_be_bytes());
        });
        read_populated_file(&Filesystem::mount(device.clone()).unwrap());
        if index < 11 {
            read_populated_file(&Filesystem::mount_rw(device.clone()).unwrap());
        } else {
            unsupported(
                Filesystem::mount_rw(device.clone()),
                &format!("unknown quota flags {bit:#x} are not maintained by writes"),
            );
        }
        assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn realtime_data_mutations_refuse_before_writes_with_named_errors() {
    let before = fixture_digest();
    let device = Probe::with_bit(offsets::FEATURES_COMPAT, 0);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let file = fs.lookup_path("/sf/data.bin").unwrap();
    let (inode, mut raw) = fs.read_inode_raw(file.ino).unwrap();
    let at = fs.inode_offset(file.ino).unwrap();
    let flags = inode.flags | fs_xfs::inode::flags::REALTIME;
    raw[fs_xfs::inode::offsets::FLAGS..fs_xfs::inode::offsets::FLAGS + 2]
        .copy_from_slice(&flags.to_be_bytes());
    raw[fs_xfs::inode::offsets::CRC..fs_xfs::inode::offsets::CRC + 4].fill(0);
    let crc = crc32c::crc32c(&raw);
    raw[fs_xfs::inode::offsets::CRC..fs_xfs::inode::offsets::CRC + 4]
        .copy_from_slice(&crc.to_le_bytes());
    drop(fs);
    let mut probe = Arc::try_unwrap(device).ok().unwrap();
    probe.inode_overlay = Some((at, raw));
    let device = Arc::new(probe);
    let fs = Filesystem::mount_rw(device.clone()).unwrap();
    let (inode, raw) = fs.read_inode_raw(file.ino).unwrap();
    assert!(
        matches!(fs.read_file(&inode, &raw), Err(Error::RealtimeDeviceAbsent { ino }) if ino == file.ino)
    );
    let expected = format!("inode {} keeps its data on the real-time device", file.ino);
    unsupported(fs.write_at(&inode, &raw, 0, b"x"), &expected);
    unsupported(fs.truncate(&inode, 1, None), &expected);
    unsupported(
        fs.truncate_to_zero(file.ino),
        &format!("{expected}, which has no allocation groups to free into"),
    );
    assert_eq!(device.writes.load(Ordering::SeqCst), 0);
    assert_eq!(fixture_digest(), before);
}
