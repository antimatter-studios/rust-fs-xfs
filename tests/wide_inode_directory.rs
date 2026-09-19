//! A directory on a volume big enough to number inodes above four bytes
//! keeps those numbers (#235).
//!
//! A short-form directory stores each entry's inode number in four
//! bytes or in eight, and which it is comes from `i8count` in its own
//! header. A directory made while the numbers were small says four, and
//! the encoder here took that as settled: a create added the new inode
//! number to the list and wrote it as four bytes whatever it was. An
//! inode above the limit came out truncated — an entry naming an inode
//! that does not exist, with nothing reported.
//!
//! An inode number is `agno << (agblklog + inopblog) | agino`, so the
//! second allocation group of a volume with one-terabyte groups starts
//! above `2^31`, which is the limit. That is an ordinary filesystem:
//! this one is 2200 GiB, sparse, and about 65 MB of it is real.
//!
//! The kernel makes the directories — it spreads them across groups, so
//! one of them lands in the second — and this driver creates a file in
//! the one that did. Then the kernel reads back the name.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Where this suite's scratch volume lives (#223).
const SUITE: &str = "wide_inode_directory";

/// `XFS_DIR2_MAX_SHORT_INUM`: the largest inode number the kernel will
/// put in a four-byte short-form entry. Above it the directory is
/// converted to eight-byte numbers and `i8count` says so.
const MAX_SHORT_INUM: u64 = 0x7fff_ffff;

/// What the four bytes actually hold, which is a different number.
///
/// Between the two, a truncating writer is lucky: the value round-trips
/// even though the kernel would never have stored it that way. Above
/// this, the top half is simply gone — so this is what the fixture
/// looks for, and it is why it needs groups of a terabyte rather than
/// merely large ones.
const FITS_IN_FOUR_BYTES: u64 = 0xffff_ffff;

/// One terabyte of 4 KiB blocks, less the eight `mkfs.xfs` will not
/// take: the largest allocation group there is, which is what puts the
/// next group's inodes above the limit.
const AGSIZE_BLOCKS: u64 = 268_435_448;

/// Enough for three of those groups. Sparse — the filesystem's own
/// metadata is what takes room, and a 64 MiB log keeps that to about
/// 65 MB.
const VOLUME_BYTES: u64 = 2200 * 1024 * 1024 * 1024;

/// How many directories to make before giving up on the kernel putting
/// one in another group. It alternates groups for directories, so the
/// second or third is usually enough.
const DIRECTORIES: u32 = 40;

#[test]
fn a_create_in_a_high_numbered_directory_keeps_the_inode_number() {
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let volume = scratch::Volume::empty(SUITE, "wide.img", VOLUME_BYTES);
    let image = volume.guest();

    let Some(built) = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -d agsize={AGSIZE_BLOCKS}b -l size=64m {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        for i in $(seq 0 {last}); do mkdir "$m/d$i"; done
        # The first directory the kernel put past what four bytes hold.
        for i in $(seq 0 {last}); do
            ino=$(stat -c %i "$m/d$i")
            if [ "$ino" -gt {FITS_IN_FOUR_BYTES} ]; then
                echo "WIDE_DIR d$i"
                echo "WIDE_INO $ino"
                break
            fi
        done
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#,
        last = DIRECTORIES - 1,
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let field = |out: &str, key: &str| -> Option<String> {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .map(|v| v.trim().to_string())
    };
    let dir_name = field(&built, "WIDE_DIR").unwrap_or_else(|| {
        panic!(
            "this kernel put all {DIRECTORIES} directories in groups whose inode \
             numbers fit in four bytes, so none of them can show a truncation and the \
             fixture proves nothing:\n{built}"
        )
    });
    let dir_ino: u64 = field(&built, "WIDE_INO")
        .expect("the guest reported one")
        .parse()
        .unwrap();
    assert!(dir_ino > FITS_IN_FOUR_BYTES);

    let made = {
        let dev = Arc::new(FileDevice::open_rw(volume.path().to_str().unwrap()).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        let dir = fs
            .lookup_path(&format!("/{dir_name}"))
            .expect("the directory the kernel put in another group");
        assert_eq!(
            dir.ino, dir_ino,
            "the driver and the kernel disagree about it"
        );
        let (ino, _) = fs
            .create_file(dir.ino, b"wide_entry", 0o100644)
            .expect("creating a file in it");
        // The new inode comes from the parent's group, so it is past
        // four bytes too — which is the case the entry has to hold.
        assert!(
            ino > FITS_IN_FOUR_BYTES,
            "the new inode is {ino}, which four bytes hold, so this tests nothing"
        );
        assert!(
            ino > MAX_SHORT_INUM,
            "and past what the kernel stores in four"
        );
        ino
    };

    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {image} "$m"; then
            echo "ENTRY_INO $(stat -c %i "$m/{dir_name}/wide_entry" 2>&1)"
            # AND THE KERNEL ADDS TO THE SAME DIRECTORY. It reads the
            # header this driver wrote and maintains it: if the count
            # were wrong, this is what would build on it.
            : > "$m/{dir_name}/kernel_made"
            echo "KERNEL_INO $(stat -c %i "$m/{dir_name}/kernel_made")"
            echo "NAMES $(ls "$m/{dir_name}" | tr '\n' ' ')"
            umount "$m" || echo UMOUNT_FAILED
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m"
        # What the reference debugger makes of the header, whichever
        # spelling of the union this xfsprogs uses.
        i8=$(xfs_db -r -c "inode {dir_ino}" -c "p u3" {image} 2>/dev/null \
             | grep -o "i8count = [0-9]*" | head -1 | awk "{{print \$3}}")
        [ -n "$i8" ] || i8=$(xfs_db -r -c "inode {dir_ino}" -c "p u" {image} 2>/dev/null \
             | grep -o "i8count = [0-9]*" | head -1 | awk "{{print \$3}}")
        echo "I8COUNT $i8"
        {repair}
        echo DONE
        "#,
        repair = repair::script(&image),
    ))
    .expect("kernel");

    assert!(
        out.contains("MOUNTED"),
        "the kernel refused the volume after the create:\n{out}"
    );
    assert_eq!(
        field(&out, "ENTRY_INO").expect("the guest reported the entry"),
        made.to_string(),
        "the name resolves to a different inode than the one the driver created — \
         which is what a truncated inode number looks like from here:\n{out}"
    );
    assert_eq!(
        field(&out, "NAMES").expect("the guest listed the directory"),
        "kernel_made wide_entry",
        "the directory holds the name this driver put in and the one the kernel did:\n{out}"
    );
    // THE COUNT, AS THE REFERENCE DEBUGGER READS IT. Two inode numbers
    // past what four bytes hold — this driver's entry and the kernel's
    // — and the parent, 128, which is not one. A driver that wrote the
    // entry wide and left the count at zero would be read as narrow by
    // anything that trusts the header.
    assert_eq!(
        field(&out, "I8COUNT").expect("xfs_db reported the header"),
        "2",
        "xfs_db counts a different number of wide inode numbers than there are:\n{out}"
    );
    repair::assert_agreed(
        &out,
        "the volume after a create in a wide-numbered directory",
    );
}
