//! What `fs.xfs write` and `mkdir` leave is a filesystem `xfs_repair`
//! accepts as it is and Linux mounts, holding exactly the bytes that went
//! in on stdin.
//!
//! The writes are log records, and each verb then releases the volume
//! clean: it writes the change home, puts the summary counters right and
//! ends the log with an unmount record. So `xfs_repair -n` grades the
//! volume after EVERY write, before any kernel has mounted it -- it
//! declines to grade an image whose log holds records
//! (`repair::assert_agreed` refuses that report rather than reading it as
//! a pass), which is how a write that left its log dirty fails here. At
//! the end the KERNEL mounts the image, reads every written file back with
//! `sha256sum`, and unmounts it, and `xfs_repair -n` grades it once more.
//!
//! THE WRITES CHAIN WITH NO KERNEL BETWEEN THEM. The driver writes only to
//! a volume whose log is clean, and each write leaves it clean, so mkdir
//! /d, then mkdir /d/e, then a file inside it run one after another on the
//! volume the last write left.
//!
//! EVERY GUEST STEP WORKS ON A COPY ON THE GUEST'S OWN DISK and copies it
//! back: a loop mount of a file on the shared folder mixes the guest's
//! page cache with the host's view of the same file, and `xfs_repair`
//! gets ENOTDIR from it. The guest's caches are dropped before each copy
//! in, so it reads the bytes the host just wrote rather than the ones it
//! wrote back itself.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_write_kernel";

/// Mount `image` with the kernel and unmount it, running `then` against
/// the mounted tree first, in the guest, on a copy on the guest's own disk
/// that is copied back. `then` runs with `$m` the mount point, from inside
/// it.
fn with_kernel(image: &str, then: &str) -> String {
    kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        img=$(mktemp -u /tmp/cli-write-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo MOUNT_OK
            ( cd "$m" && {then} )
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_REFUSED
        fi
        rmdir "$m"
        cp --sparse=always "$img" {image}
        {repair}
        rm -f "$img"
        sync
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ))
}

/// `xfs_repair -n` on `image` as the write left it -- no kernel has
/// mounted it, so nothing has replayed anything -- must find nothing
/// wrong, and the tool must call the log clean.
#[track_caller]
fn graded(image: &str, after: &str) {
    let out = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        img=$(mktemp -u /tmp/cli-write-XXXXXX.img)
        cp --sparse=always {image} "$img"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    repair::assert_agreed(
        &out,
        &format!("the volume after {after}, before any replay"),
    );
    let dirty = stdout(&ok(tool("fs.xfs").args([image, "get", "dirty", "--text"])));
    assert_eq!(
        dirty.trim(),
        "false",
        "{after} left the log holding records"
    );
}

#[track_caller]
fn wrote(image: &str, path: &str, bytes: &[u8]) {
    let out = fs_write(image, path, bytes);
    assert!(
        out.status.success(),
        "fs.xfs write {path} failed ({:?}): {}",
        out.status.code(),
        stderr(&out)
    );
}

#[track_caller]
fn made_dir(image: &str, path: &str) {
    ok(tool("fs.xfs").args([image, "mkdir", path]));
}

#[test]
fn the_kernel_replays_what_the_write_verbs_logged_and_xfs_repair_accepts_it() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-v5.img", std::process::id()),
    );
    let image = copy.path().to_str().unwrap().to_string();
    let image = image.as_str();

    // One journalled write after another on the volume the last one left,
    // each graded by xfs_repair before any kernel has seen it.
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("/f1", pattern(1, 1)),
        ("/f4096", pattern(4096, 2)),
        ("/f4097", pattern(4097, 3)),
        ("/big", pattern(1 << 20, 4)),
    ];
    for (path, bytes) in &files {
        wrote(image, path, bytes);
        graded(image, &format!("write {path}"));
    }
    made_dir(image, "/d");
    graded(image, "mkdir /d");
    made_dir(image, "/d/e");
    graded(image, "mkdir /d/e");
    let deep = pattern(5000, 5);
    wrote(image, "/d/e/deep", &deep);
    graded(image, "write /d/e/deep");
    let filled = pattern(3000, 6);
    wrote(image, "/empty", &filled);
    graded(image, "write /empty (an empty file given contents)");
    let same = pattern(262_144, 7);
    wrote(image, "/medium.bin", &same);
    graded(image, "write /medium.bin (a same-length overwrite)");

    // A write too large for any free run is refused, and the file it had
    // created is unlinked again: two records, written home, leaving a
    // volume with no /huge in it.
    let huge = fs_write(image, "/huge", &vec![0u8; 100_000_000]);
    assert_eq!(huge.status.code(), Some(3), "{}", stderr(&huge));
    graded(image, "the refused write /huge");

    // Now the kernel reads it all.
    let mut expect: Vec<(String, String)> = files
        .iter()
        .map(|(p, b)| (p.trim_start_matches('/').to_string(), sha256_hex(b)))
        .collect();
    expect.push(("d/e/deep".into(), sha256_hex(&deep)));
    expect.push(("empty".into(), sha256_hex(&filled)));
    expect.push(("medium.bin".into(), sha256_hex(&same)));
    let names: Vec<&str> = expect.iter().map(|(p, _)| p.as_str()).collect();
    let out = with_kernel(
        image,
        &format!(
            "sha256sum {}; stat -c 'MODE %n %F %a' d d/e; \
             [ -e huge ] && echo HUGE_PRESENT || echo HUGE_ABSENT; \
             ls -A d/e | sed 's/^/IN_DE /'",
            names.join(" ")
        ),
    );
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount it:\n{out}"
    );
    for (path, sum) in &expect {
        let line = out
            .lines()
            .find(|l| l.ends_with(&format!("  {path}")))
            .unwrap_or_else(|| panic!("the kernel printed no sha256 for {path}:\n{out}"));
        assert!(
            line.starts_with(sum.as_str()),
            "{path}: the kernel reads different bytes from those written on stdin \
             (want {sum}):\n{line}"
        );
    }
    assert!(out.contains("MODE d directory 755"), "{out}");
    assert!(out.contains("MODE d/e directory 755"), "{out}");
    assert!(
        out.contains("HUGE_ABSENT"),
        "the refused write left /huge:\n{out}"
    );
    let in_de: Vec<&str> = out.lines().filter(|l| l.starts_with("IN_DE ")).collect();
    assert_eq!(in_de, vec!["IN_DE deep"], "{out}");
    repair::assert_agreed(&out, "the volume after every write verb and a kernel mount");

    // And after the kernel's unmount the tool calls the log clean still.
    let get = stdout(&ok(tool("fs.xfs").args([image, "get", "dirty", "--text"])));
    assert_eq!(
        get.trim(),
        "false",
        "the volume reads as dirty after the kernel's unmount"
    );
}

#[test]
fn a_same_length_overwrite_on_v4_is_what_the_kernel_reads() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v4.img"),
        &format!("{}-v4.img", std::process::id()),
    );
    let image = copy.path().to_str().unwrap();
    let bytes = pattern(262_144, 8);
    wrote(image, "/medium.bin", &bytes);
    let out = with_kernel(image, "sha256sum medium.bin");
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount it:\n{out}"
    );
    assert!(
        out.contains(&format!("{}  medium.bin", sha256_hex(&bytes))),
        "the kernel reads different bytes from those overwritten:\n{out}"
    );
    repair::assert_agreed(&out, "a v4 volume after an overwrite in place");
}
