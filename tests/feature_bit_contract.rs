//! Feature-mask refusals over an independently formatted fixture.
//! Only the superblock sector is overlaid; the source image stays read-only.

mod common;

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::superblock::{incompat, offsets, ro_compat};
use fs_xfs::{Error, Filesystem};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct Probe {
    source: FileDevice,
    header: Vec<u8>,
    writes: AtomicUsize,
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
        })
    }
}

#[test]
fn active_quota_accounting_is_readable_but_refuses_all_write_mounts() {
    for bit in [1u16, 1 << 3, 1 << 6] {
        let device = Probe::with_overlay(|header| {
            header[offsets::QFLAGS..offsets::QFLAGS + 2].copy_from_slice(&bit.to_be_bytes());
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
