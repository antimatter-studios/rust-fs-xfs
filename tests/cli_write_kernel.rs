//! What `fs.xfs write` and `mkdir` leave is a filesystem Linux replays and
//! `xfs_repair` accepts, holding exactly the bytes that went in on stdin.
//!
//! The writes are log records: "nothing on disk is touched: the record is
//! the change". So the check is two-stage, in the harness guest, as the
//! driver's own write oracles are: the KERNEL mounts the image, which
//! replays the records, and reads every written file back with
//! `sha256sum`; then, on the unmounted image, `xfs_repair -n` must find
//! nothing. `xfs_repair` runs only after the replay, because it declines
//! to grade an image whose log holds records (`repair::assert_agreed`
//! refuses that report rather than reading it as a pass).
//!
//! THE REPLAY IS WHAT LETS THE NEXT WRITE HAPPEN. The driver writes only
//! to a volume whose log is clean, so a chain of writes -- mkdir /d, then
//! mkdir /d/e, then a file inside it -- is one write, one kernel mount,
//! and the next write, on the volume the kernel left.
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

/// Mount `image` with the kernel -- replaying its log -- and unmount it,
/// then run `then` against the mounted tree first, in the guest, on a copy
/// on the guest's own disk that is copied back. `then` runs with `$m` the
/// mount point, from inside it.
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

/// Replay `image`'s log with the kernel and require the result clean.
#[track_caller]
fn replay(image: &str, after: &str) -> String {
    let out = with_kernel(image, "true");
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount the volume after {after}:\n{out}"
    );
    repair::assert_agreed(&out, &format!("the volume after {after}, replayed"));
    out
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

    // One journalled write, one replay, and on to the next: each write
    // needs a clean log, and each replay is also a check.
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("/f1", pattern(1, 1)),
        ("/f4096", pattern(4096, 2)),
        ("/f4097", pattern(4097, 3)),
        ("/big", pattern(1 << 20, 4)),
    ];
    for (path, bytes) in &files {
        wrote(image, path, bytes);
        replay(image, &format!("write {path}"));
    }
    made_dir(image, "/d");
    replay(image, "mkdir /d");
    made_dir(image, "/d/e");
    replay(image, "mkdir /d/e");
    let deep = pattern(5000, 5);
    wrote(image, "/d/e/deep", &deep);
    replay(image, "write /d/e/deep");
    let filled = pattern(3000, 6);
    wrote(image, "/empty", &filled);
    replay(image, "write /empty (an empty file given contents)");
    let same = pattern(262_144, 7);
    wrote(image, "/medium.bin", &same);

    // A write too large for any free run is refused, and the file it had
    // created is unlinked again: two records the kernel has to replay into
    // a volume with no /huge in it.
    let huge = fs_write(image, "/huge", &vec![0u8; 100_000_000]);
    assert_eq!(huge.status.code(), Some(3), "{}", stderr(&huge));

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
    repair::assert_agreed(&out, "the volume after every write verb, replayed");

    // And after the kernel's unmount the tool calls the log clean again.
    let get = stdout(&ok(tool("fs.xfs").args([image, "get", "dirty", "--text"])));
    assert_eq!(
        get.trim(),
        "false",
        "the replayed volume still reads as dirty"
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
