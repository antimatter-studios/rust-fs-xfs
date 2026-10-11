//! A volume this driver wrote and unmounted has a clean log (#439).
//!
//! Every write is a checkpoint in the log, so without an unmount a volume
//! the driver wrote always has a dirty log: only a mount that replays it
//! sees the writes, `xfs_repair` will not grade it, and a writable mount
//! here refuses it. `Filesystem::unmount` writes everything home, puts the
//! superblock's summary counters right, and ends the log with an unmount
//! record. What `xfs_repair -n` and `xfs_logprint` make of it is
//! `tests/clean_unmount_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::log::LogState;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

struct Sparse(Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]))[within..within + n]
                .copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A volume with directories, files of several extents and a hole, and
/// hard links written into it, then unmounted.
fn written_and_unmounted() -> (Arc<Sparse>, Vec<(String, Vec<u8>)>) {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    let root = fs.superblock().rootino;
    let (dir, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let mut files = Vec::new();
    for i in 0..40 {
        let name = format!("file-{i:02}");
        let (ino, _) = fs
            .create_file(dir, name.as_bytes(), 0o100644)
            .expect("create");
        let data = vec![i as u8 + 1; 3000 + i * 997];
        fs.write(ino, 0, &data).expect("write");
        files.push((format!("/d/{name}"), data));
    }
    let (big, _) = fs.create_file(root, b"holes", 0o100644).expect("create");
    fs.write(big, 1 << 20, &[7u8; 9000])
        .expect("write past a hole");
    let mut holes = vec![0u8; (1 << 20) + 9000];
    holes[1 << 20..].fill(7);
    files.push(("/holes".into(), holes));
    fs.link(big, dir, b"another-name").expect("link");
    fs.unmount().expect("unmount");
    (dev, files)
}

#[test]
fn an_unmounted_volume_has_a_clean_log() {
    let (dev, _) = written_and_unmounted();
    let fs = Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount");
    let state = fs_xfs::log::inspect(dev.as_ref(), fs.superblock()).expect("inspect");
    assert!(
        matches!(state, LogState::CleanlyUnmounted),
        "the log after an unmount is {state:?}"
    );
    // A writable mount refuses a dirty log, and takes a clean one.
    drop(fs);
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("a writable mount of a clean log");
}

#[test]
fn what_was_written_reads_back_and_the_counters_agree() {
    let (dev, files) = written_and_unmounted();
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>).expect("mount");
    for (path, data) in &files {
        assert!(
            &fs.read_path(path).expect(path) == data,
            "{path} reads other bytes"
        );
    }
    assert_eq!(
        fs.lookup_path("/d/another-name").unwrap().ino,
        fs.lookup_path("/holes").unwrap().ino
    );
    let report = fs_xfs::check::check(&fs);
    assert!(
        report.is_clean(),
        "an unmounted volume has findings: {:?}",
        report.findings
    );
}
