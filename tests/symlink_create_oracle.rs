//! A symlink this driver logs is one the kernel replays and reads (#462).
//!
//! A short target is kept inline in the inode, and a long one in a run of
//! blocks behind a v5 header naming its owner, its address and how much
//! of the target the run holds, with one checksum over the whole run.
//! Every part of that header is something the kernel's verifier checks
//! and this driver's own reader might not, so the judge is the kernel.
//!
//! # The shape of the proof
//!
//! The guest's own `mkfs.xfs` makes two volumes: 4 KiB blocks, where the
//! longest target fits one block, and 1 KiB blocks, where it needs a
//! run of two with one header. On each the driver makes a directory and
//! three symlinks: an inline target, a remote one and the longest one
//! XFS stores. It stops without unmounting, so only the log holds them.
//! Then:
//!
//! - this driver's read-only mount, which replays the log in memory,
//!   reads each target back;
//! - the kernel mounts the volume, replaying the log, and `readlink`
//!   returns each target byte for byte, with the type, mode, link count
//!   and size a symlink has;
//! - after the kernel unmounts, `xfs_repair -n` finds nothing wrong.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::{guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "symlink_create_oracle";

/// A target `len` bytes long, path-shaped and not repetitive enough for
/// a misplaced chunk to read back the same.
fn long_target(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| {
            if i % 37 == 36 {
                b'/'
            } else {
                b'a' + (i % 26) as u8
            }
        })
        .collect()
}

/// The links each volume gets: `(path, target)`.
fn links() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("d/short", b"../somewhere/else".to_vec()),
        ("d/long", long_target(900)),
        ("most", long_target(1024)),
    ]
}

fn symlinks_on(blocksize: u32) {
    let case = format!("{blocksize}-byte blocks");
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-{blocksize}.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let formatted = kernel_run(&format!(
        "mkfs.xfs -q -f -b size={blocksize} {image}\necho DONE\n"
    ));
    assert!(formatted.contains("DONE"), "{case}: {formatted}");

    let mut made = Vec::new();
    {
        let dev = FileDevice::open_rw(volume.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        let (dir, _) = fs
            .create_directory(root, b"d", 0o755)
            .unwrap_or_else(|e| panic!("{case}: mkdir d: {e}"));
        for (path, target) in links() {
            let (parent, name) = match path.split_once('/') {
                Some((_, name)) => (dir, name),
                None => (root, path),
            };
            let (ino, _) = fs
                .create_symlink(parent, name.as_bytes(), &target)
                .unwrap_or_else(|e| panic!("{case}: symlink {path}: {e}"));
            made.push((path, ino, target));
        }
        // Dropped without unmounting: the records are all the volume has
        // of these links, and the kernel has to replay them.
    }

    // This driver's own reading first, replaying the log in memory.
    {
        let dev = FileDevice::open(volume.path()).expect("open read-only");
        let fs = Filesystem::mount(Arc::new(dev)).expect("a mount that replays the log");
        for (path, ino, target) in &made {
            let (inode, raw) = fs.read_inode_raw(*ino).expect(path);
            let read = fs
                .read_link(&inode, &raw)
                .unwrap_or_else(|e| panic!("{case}: reading {path} back: {e}"));
            assert_eq!(&read, target, "{case}: {path} read back by this driver");
        }
    }

    // The targets, where the guest can compare against them.
    let dir = scratch::dir(SUITE);
    let mut checks = String::new();
    let mut expected_files = Vec::new();
    for (i, (path, _, target)) in made.iter().enumerate() {
        let file = dir.join(format!("{}-{blocksize}-target-{i}", std::process::id()));
        std::fs::write(&file, target).expect("write the expected target");
        let want = guest_quote(&scratch::guest_path(&file));
        checks.push_str(&format!(
            r#"
            readlink "$m/{path}" | tr -d '\n' > /tmp/got
            cmp -s /tmp/got {want} && echo "TARGET_OK {path}"
            echo "STAT {path} $(stat -c '%F %a %h %s' "$m/{path}")"
            "#
        ));
        expected_files.push(file);
    }

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {checks}
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        {repair}
        rm -f "$img"
        echo "DONE"
        "#,
        repair = repair::script("\"$img\""),
    );
    let out = kernel_run(&script);
    for file in &expected_files {
        std::fs::remove_file(file).ok();
    }

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused the volume after the symlinks were logged:\n{out}"
    );
    assert!(
        !out.contains("UMOUNT_FAILED"),
        "{case}: the kernel could not unmount the volume:\n{out}"
    );
    for (path, _, target) in &made {
        assert!(
            out.contains(&format!("TARGET_OK {path}")),
            "{case}: the kernel read another target for {path}:\n{out}"
        );
        assert!(
            out.contains(&format!("STAT {path} symbolic link 777 1 {}", target.len())),
            "{case}: the kernel does not see {path} as a {}-byte symlink:\n{out}",
            target.len()
        );
    }
    repair::assert_agreed(&out, &format!("{case}: a volume holding logged symlinks"));
}

#[test]
fn symlinks_on_4k_blocks_are_replayed_and_read_by_the_kernel() {
    symlinks_on(4096);
}

#[test]
fn symlinks_on_1k_blocks_are_replayed_and_read_by_the_kernel() {
    symlinks_on(1024);
}
