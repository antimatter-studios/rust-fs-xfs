//! `fsck.xfs` checks a volume's realtime metadata, and agrees with
//! `xfs_repair -n` about what is wrong with it (#381).
//!
//! The volume is made in the harness guest with a realtime device and
//! `rtinherit`, so the kernel puts the files it writes on the realtime
//! section, and their allocation is recorded in the realtime bitmap and
//! summary on the data device. That volume must be clean to the reference
//! and to `fsck.xfs`, which reads the data device alone: the bitmap, the
//! summary and every realtime file's extent map are there.
//!
//! Then one realtime structure is damaged at a time:
//!
//! - **the free count**: `sb_frextents` raised by one, with `xfs_db`;
//! - **the bitmap**: a bit for an extent a file maps set to free, by hand;
//! - **the summary**: one count raised by one, by hand;
//! - **an extent**: a file's realtime extent moved past the section, with
//!   `xfs_db`.
//!
//! The bitmap and summary of a volume without realtime groups carry no
//! checksum, so a byte edited by hand is the damage it names. Each case
//! must be damage to `xfs_repair -n -r`, and `fsck.xfs` must exit 4 and
//! report the code.

mod cli_support;
mod common;

use cli_support::*;
use common::{kernel_run, oracle, repair, scratch, share};
use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;

const SUITE: &str = "realtime_check_oracle";

/// A data image and its realtime image, with realtime files written by
/// the kernel.
fn realtime_volume(tag: &str) -> (scratch::Volume, scratch::Volume) {
    assert!(
        share().is_dir(),
        "no {}: `chore fixtures` makes it",
        share().display()
    );
    let pid = std::process::id();
    let data = scratch::Volume::empty(SUITE, &format!("{pid}-{tag}-data.img"), 300 * 1024 * 1024);
    let rt = scratch::Volume::empty(SUITE, &format!("{pid}-{tag}-rt.img"), 64 * 1024 * 1024);
    let (data_name, rt_name) = (data.guest(), rt.guest());
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -r rtdev={rt_name} -d rtinherit=1 {data_name} 2>&1 && echo MKFS_OK
        loop=$(losetup -f --show {rt_name})
        m=$(mktemp -d)
        mount -o loop,rtdev="$loop" {data_name} "$m" && echo MOUNT_OK
        head -c 300000 /dev/urandom > "$m/one"
        for i in 0 3 7; do
            xfs_io -f -c "pwrite -q -S 0x$((i + 17)) $((i * 65536)) 4096" "$m/sparse"
        done
        head -c 2000000 /dev/urandom > "$m/two"
        rm "$m/one"
        sync
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        losetup -d "$loop"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the realtime volume failed:\n{built}"
    );
    (data, rt)
}

fn repair_says(data: &scratch::Volume, rt: &scratch::Volume) -> (bool, String) {
    let out = oracle("xfs_repair")
        .args(["-n", "-r"])
        .arg(rt.path())
        .arg(data.path())
        .output();
    let report = out.repair_report();
    assert!(
        !repair::was_blind(&report),
        "xfs_repair -n declined to look:\n{report}"
    );
    (out.ok(), report)
}

fn fsck(data: &scratch::Volume) -> (Option<i32>, String) {
    let out = tool("fsck.xfs")
        .args(["--text"])
        .arg(data.path())
        .output()
        .expect("spawn fsck.xfs");
    (
        out.status.code(),
        format!("{}{}", stdout(&out), stderr(&out)),
    )
}

fn assert_found(name: &str, data: &scratch::Volume, rt: &scratch::Volume, code: &str) {
    let (clean, report) = repair_says(data, rt);
    assert!(
        !clean,
        "{name}: xfs_repair -n calls this clean, so it is not damage:\n{report}"
    );
    let (status, text) = fsck(data);
    assert_eq!(status, Some(4), "{name}: fsck.xfs exit\n{text}");
    assert!(
        text.contains(&format!(": {code}: ")),
        "{name}: fsck.xfs did not report {code}; xfs_repair -n said:\n{report}\nfsck.xfs said:\n{text}"
    );
}

/// Where the first block of inode `ino`'s data is in the data image, and
/// the superblock, read with this driver: the reference judges the
/// result, so how it is located does not matter.
fn first_block_of(data: &scratch::Volume, ino: impl Fn(&fs_xfs::Superblock) -> u64) -> u64 {
    let dev = FileDevice::open(data.path().to_str().unwrap()).expect("open");
    let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");
    let sb = fs.superblock().clone();
    let (inode, raw) = fs.read_inode_raw(ino(&sb)).expect("inode");
    let extents = fs.data_extents(&inode, &raw).expect("extents");
    sb.fsblock_offset(extents.first().expect("an extent").startblock)
}

fn patch(data: &scratch::Volume, at: u64, edit: impl FnOnce(&mut [u8; 4])) {
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(data.path())
        .expect("open the image");
    let mut word = [0u8; 4];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.read_exact(&mut word).unwrap();
    edit(&mut word);
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&word).unwrap();
}

#[test]
fn a_realtime_volume_the_kernel_wrote_is_clean() {
    let (data, rt) = realtime_volume("clean");
    let (_, report) = repair_says(&data, &rt);
    repair::assert_agreed(&report, "the realtime volume the kernel wrote");
    let (status, text) = fsck(&data);
    assert_eq!(
        status,
        Some(0),
        "fsck.xfs calls the realtime volume damaged:\n{text}"
    );
}

#[test]
fn a_wrong_free_extent_count_is_found() {
    let (data, rt) = realtime_volume("frextents");
    let shown = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "p frextents"])
        .arg(data.path())
        .output();
    let n: u64 = shown
        .stdout
        .split('=')
        .nth(1)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| panic!("no frextents:\n{}", shown.stdout));
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            "sb 0",
            "-c",
            &format!("write -d frextents {}", n + 1),
        ])
        .arg(data.path())
        .output();
    assert!(edit.ok(), "xfs_db: {}{}", edit.stdout, edit.stderr);
    assert_found(
        "sb_frextents off by one",
        &data,
        &rt,
        "counter.sb.frextents",
    );
}

#[test]
fn a_bitmap_bit_freeing_a_mapped_extent_is_found() {
    let (data, rt) = realtime_volume("bitmap");
    // Realtime extent 0 is mapped: the first file written starts there,
    // and the one removed afterwards came later.
    let at = first_block_of(&data, |sb| sb.rbmino);
    let used = {
        let dev = FileDevice::open(data.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");
        let sb = fs.superblock().clone();
        let ino = fs.lookup_path("/sparse").expect("sparse").ino;
        let (inode, raw) = fs.read_inode_raw(ino).expect("inode");
        let e = fs.data_extents(&inode, &raw).expect("extents")[0];
        e.startblock / u64::from(sb.rextsize)
    };
    patch(&data, at + used / 32 * 4, |w| {
        let mut v = u32::from_le_bytes(*w);
        v |= 1 << (used % 32);
        *w = v.to_le_bytes();
    });
    assert_found(
        "bitmap bit set over a mapped extent",
        &data,
        &rt,
        "rt.bitmap",
    );
}

#[test]
fn a_wrong_summary_count_is_found() {
    let (data, rt) = realtime_volume("summary");
    let at = first_block_of(&data, |sb| sb.rsumino);
    patch(&data, at, |w| {
        *w = (u32::from_le_bytes(*w) + 1).to_le_bytes();
    });
    assert_found("summary count raised", &data, &rt, "rt.summary");
}

#[test]
fn a_realtime_extent_past_the_section_is_found() {
    let (data, rt) = realtime_volume("extent");
    let ino = {
        let dev = FileDevice::open(data.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");
        fs.lookup_path("/sparse").expect("sparse").ino
    };
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            &format!("inode {ino}"),
            "-c",
            "write -d u3.bmx[0].startblock 100000000",
        ])
        .arg(data.path())
        .output();
    assert!(edit.ok(), "xfs_db: {}{}", edit.stdout, edit.stderr);
    assert_found("realtime extent past the section", &data, &rt, "rt.extent");
}
