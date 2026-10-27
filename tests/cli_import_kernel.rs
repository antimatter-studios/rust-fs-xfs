//! `fs.xfs import`, judged by THE KERNEL and by xfsprogs.
//!
//! The host imports into copies of the kernel-made v5 CLI image; the harness
//! guest then mounts each copy with Linux, which replays whatever log is
//! left, digests the files with `sha256sum` and unmounts, and `xfs_repair
//! -n` grades the volume. The digests the kernel reports are compared with
//! the SHA-256 of the local source files, computed here, not by the tool.
//!
//! - a 64 MiB file and a one-byte file read back by the kernel with the
//!   source's SHA-256, and the volume is clean;
//! - a tree imported with `--recursive` reads back file by file;
//! - an import that runs out of space on a nearly full image, replacing a
//!   file, leaves the old file byte-identical when the kernel reads it, no
//!   staged name behind, and the volume clean;
//! - an import killed (SIGKILL) at several moments, replacing a file, leaves
//!   under that name either the old file or the whole new one, never part
//!   of one, and the volume clean once the kernel has replayed its log. A
//!   staged name left by the kill is allowed, and is reported.
//! - repeated SIGKILLs during a large replacement, at seeded moments
//!   inside the time an uninterrupted one takes, leave every image
//!   replayable and unmountable, with no partial file and clean
//!   allocation counters; and the same holds for `fs.xfs write` (#581).
//!
//! EVERY GUEST STEP WORKS ON A COPY ON THE GUEST'S OWN DISK, for the
//! reason `cli_write_kernel.rs` gives: a loop mount of a file on the
//! shared folder mixes the guest's page cache with the host's view of it.

mod cli_support;
mod common;

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_import_kernel";

/// What the kernel made of `image`: the SHA-256 it reads for each of
/// `files`, and the names in the root, after it has replayed the log.
struct Graded {
    digests: Vec<(String, String)>,
    root: Vec<String>,
}

fn grade(image: &str, files: &[&str]) -> Graded {
    let sums: String = files
        .iter()
        .map(|f| {
            format!("echo \"SHA $(sha256sum \"$work/m{f}\" 2>/dev/null | cut -d' ' -f1) {f}\"\n")
        })
        .collect();
    let out = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        if mount -o loop,nouuid "$work/image" "$work/m"; then
            echo MOUNT_OK
            {sums}
            for n in $(ls -A "$work/m"); do echo "ROOT $n"; done
            umount "$work/m" || (echo UMOUNT_FAILED; dmesg | tail -15)
        else
            echo MOUNT_REFUSED
        fi
        {repair}
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$work/image\""),
    ));
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount the volume:\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    repair::assert_agreed(
        &out,
        "the volume fs.xfs import left, after the kernel's replay",
    );
    Graded {
        digests: out
            .lines()
            .filter_map(|l| l.strip_prefix("SHA "))
            .filter_map(|l| l.split_once(' '))
            .map(|(sum, path)| (path.to_string(), sum.to_string()))
            .collect(),
        root: out
            .lines()
            .filter_map(|l| l.strip_prefix("ROOT "))
            .map(String::from)
            .collect(),
    }
}

impl Graded {
    fn sum(&self, path: &str) -> &str {
        &self
            .digests
            .iter()
            .find(|(p, _)| p == path)
            .unwrap_or_else(|| panic!("the kernel digested no {path}"))
            .1
    }

    fn staged(&self) -> Vec<&String> {
        self.root
            .iter()
            .filter(|n| n.starts_with(".fs-xfs-import-"))
            .collect()
    }
}

fn copy(base: &Path, tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(SUITE, base, &format!("{}-{tag}.img", std::process::id()))
}

fn source(name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch::dir(SUITE).join(format!("{}-{name}", std::process::id()));
    std::fs::write(&path, bytes).expect("write a source file");
    path
}

#[test]
fn an_import_reads_back_through_the_kernel_with_the_digest_sent() {
    let volume = copy(&fixture("xfscli-v5.img"), "sizes");
    let image = volume.path().to_str().unwrap();

    let big = pattern(64 << 20, 11);
    let big_path = source("big", &big);
    let out = ok(tool("fs.xfs").args([
        image,
        "import",
        "--verify",
        big_path.to_str().unwrap(),
        "/big.bin",
    ]));
    assert_eq!(json_field(&stdout(&out), "sha256"), sha256_hex(&big));
    assert_eq!(json_field(&stdout(&out), "verified"), "true");
    let one_path = source("one", b"z");
    ok(tool("fs.xfs").args([image, "import", one_path.to_str().unwrap(), "/one.bin"]));

    let tree = scratch::dir(SUITE).join(format!("{}-tree", std::process::id()));
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(tree.join("a/b")).unwrap();
    let leaf = pattern(300_000, 12);
    std::fs::write(tree.join("a/b/leaf.bin"), &leaf).unwrap();
    std::fs::write(tree.join("top.txt"), b"top\n").unwrap();
    let out = ok(tool("fs.xfs").args([
        image,
        "import",
        "--recursive",
        tree.to_str().unwrap(),
        "/tree",
    ]));
    assert_eq!(json_field(&stdout(&out), "failed"), "0", "{}", stdout(&out));

    let graded = grade(
        image,
        &[
            "/big.bin",
            "/one.bin",
            "/tree/a/b/leaf.bin",
            "/tree/top.txt",
        ],
    );
    assert_eq!(graded.sum("/big.bin"), sha256_hex(&big), "64 MiB file");
    assert_eq!(graded.sum("/one.bin"), sha256_hex(b"z"), "one byte file");
    assert_eq!(graded.sum("/tree/a/b/leaf.bin"), sha256_hex(&leaf));
    assert_eq!(graded.sum("/tree/top.txt"), sha256_hex(b"top\n"));
    assert!(
        graded.staged().is_empty(),
        "staged names left: {:?}",
        graded.root
    );
    let _ = std::fs::remove_dir_all(&tree);
}

#[test]
fn an_import_that_runs_out_of_space_leaves_the_old_file_and_a_clean_volume() {
    let volume = copy(&fixture("xfscli-v5.img"), "full");
    let image = volume.path().to_str().unwrap();
    let old = pattern(50_000, 21);
    let old_path = source("old", &old);
    ok(tool("fs.xfs").args([image, "import", old_path.to_str().unwrap(), "/old.bin"]));

    // 400 MiB of zeros, as a hole: more than the 320 MiB volume holds.
    let huge = scratch::dir(SUITE).join(format!("{}-huge", std::process::id()));
    std::fs::File::create(&huge)
        .and_then(|f| f.set_len(400 << 20))
        .expect("make the oversized source");
    let out = tool("fs.xfs")
        .args([
            image,
            "import",
            "--replace",
            huge.to_str().unwrap(),
            "/old.bin",
        ])
        .output()
        .expect("run fs.xfs import");
    let _ = std::fs::remove_file(&huge);
    assert!(
        !out.status.success(),
        "an import larger than the volume succeeded: {}",
        stdout(&out)
    );
    assert!(
        stderr(&out).contains("no space"),
        "the failure does not say the volume is full: {}",
        stderr(&out)
    );

    let graded = grade(image, &["/old.bin"]);
    assert_eq!(
        graded.sum("/old.bin"),
        sha256_hex(&old),
        "the old file changed when its replacement ran out of space"
    );
    assert!(
        graded.staged().is_empty(),
        "staged names left: {:?}",
        graded.root
    );
}

/// Whether `image` holds a staged file of exactly `len` bytes and a clean
/// log: the moment after `persist` (the volume released clean) and before
/// `publish` (the rename). Read through a fresh read-only open, which never
/// changes the image.
fn persisted_not_published(image: &str, len: u64) -> bool {
    let listing = tool("fs.xfs").args([image, "ls", "/"]).output();
    let dirty = tool("fs.xfs")
        .args([image, "get", "dirty", "--text"])
        .output();
    let (Ok(listing), Ok(dirty)) = (listing, dirty) else {
        return false;
    };
    let listing = stdout(&listing);
    String::from_utf8_lossy(&dirty.stdout).trim() == "false"
        && listing.contains(".fs-xfs-import-")
        && listing.contains(&format!("\"size\": {len},"))
}

#[test]
fn an_import_killed_between_persist_and_publish_leaves_the_old_file_and_a_clean_volume() {
    let base = copy(&fixture("xfscli-v5.img"), "kill-base");
    let old = pattern(70_000, 31);
    let old_path = source("kill-old", &old);
    ok(tool("fs.xfs").args([
        base.path().to_str().unwrap(),
        "import",
        old_path.to_str().unwrap(),
        "/victim.bin",
    ]));
    let new = pattern(160 << 20, 32);
    let new_path = source("kill-new", &new);

    // The window between persist and publish is the read-back of the whole
    // staged file under --verify. It is found by watching the image, not by
    // guessing a delay, and tried again when a run was too quick to catch.
    for attempt in 1..=5 {
        let victim = copy(base.path(), &format!("kill-{attempt}"));
        let image = victim.path().to_str().unwrap();
        let mut child = tool("fs.xfs")
            .args([
                image,
                "import",
                "--replace",
                "--verify",
                new_path.to_str().unwrap(),
                "/victim.bin",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fs.xfs import");
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let mut caught = false;
        while std::time::Instant::now() < deadline {
            if child.try_wait().expect("poll the import").is_some() {
                break;
            }
            if persisted_not_published(image, new.len() as u64) {
                caught = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let _ = child.kill();
        let status = child.wait().expect("wait for the killed import");
        if !caught || status.code().is_some() {
            eprintln!("attempt {attempt}: the import was past the window; trying again");
            continue;
        }

        let graded = grade(image, &["/victim.bin"]);
        assert_eq!(
            graded.sum("/victim.bin"),
            sha256_hex(&old),
            "an import killed between persist and publish changed the old file"
        );
        for name in graded.staged() {
            eprintln!("killed between persist and publish: staged name left, reported: {name}");
        }
        return;
    }
    panic!("no attempt caught the import between persist and publish");
}

/// How long `args` takes, uninterrupted, on a copy of `base`: the window
/// a kill has to land in. A fixed window let a fast runner finish most
/// imports before the kill, which tested nothing.
fn uninterrupted(base: &Path, tag: &str, args: &[&str]) -> Duration {
    let volume = copy(base, &format!("{tag}-timed"));
    let started = std::time::Instant::now();
    ok(tool("fs.xfs")
        .arg(volume.path())
        .args(command_of(args))
        .stdin(stdin_of(args)));
    started.elapsed()
}

/// `args` up to a `<`, which names the file given to stdin.
fn command_of<'a>(args: &[&'a str]) -> Vec<&'a str> {
    args.iter().take_while(|a| **a != "<").copied().collect()
}

/// The source `fs.xfs write` reads, for the loops that run it.
fn stdin_of(args: &[&str]) -> Stdio {
    match args.iter().position(|a| *a == "<") {
        Some(at) => Stdio::from(std::fs::File::open(args[at + 1]).expect("open the write source")),
        None => Stdio::null(),
    }
}

/// SIGKILL `fs.xfs <copy> <args>` at `attempts` seeded moments inside
/// `window`, have the kernel replay, unmount and `xfs_repair -n` each copy,
/// and hand each graded copy to `check`. A copy that fails is kept, with
/// the guest's output in the panic.
fn kill_loop(
    base: &Path,
    tag: &str,
    args: &[&str],
    window: Duration,
    attempts: usize,
    digests: &[&str],
    check: impl Fn(usize, &Graded),
) {
    let window_ms = (window.as_millis() as u64).max(1);
    let mut seed = u64::from(std::process::id()) ^ 0x581;
    let mut killed = 0;
    for attempt in 0..attempts {
        let victim = copy(base, &format!("{tag}-{attempt}"));
        let mut child = tool("fs.xfs")
            .arg(victim.path())
            .args(command_of(args))
            .stdin(stdin_of(args))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fs.xfs");
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        std::thread::sleep(Duration::from_millis((seed >> 16) % window_ms));
        let (status, was_killed) = match child.try_wait().expect("poll fs.xfs") {
            Some(status) => (status, false),
            None => {
                child.kill().expect("SIGKILL the in-flight fs.xfs");
                killed += 1;
                (child.wait().expect("wait for the killed fs.xfs"), true)
            }
        };
        assert!(
            was_killed || status.success(),
            "{tag} attempt {attempt}: fs.xfs exited unexpectedly before the kill: {status}"
        );
        // The kernel replays the image after each kill, unmounts it, and
        // xfs_repair -n checks every superblock and allocation counter.
        let graded = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            grade(victim.path().to_str().unwrap(), digests)
        })) {
            Ok(graded) => graded,
            Err(panic) => {
                let evidence = scratch::dir(SUITE)
                    .join(format!("{}-{tag}-{attempt}-failed.img", std::process::id()));
                std::fs::copy(victim.path(), &evidence)
                    .expect("keep the image that failed kernel replay");
                eprintln!(
                    "{tag} attempt {attempt}: kept failed image at {}",
                    evidence.display()
                );
                std::panic::resume_unwind(panic);
            }
        };
        check(attempt, &graded);
    }
    assert!(
        killed >= attempts / 2,
        "{tag}: only {killed}/{attempts} runs were still going at the kill point \
         (window {window_ms} ms)"
    );
}

#[test]
fn arbitrary_kills_during_import_leave_a_replayable_volume() {
    const ATTEMPTS: usize = 200;
    let base = copy(&fixture("xfscli-v5.img"), "arbitrary-kill-base");
    let old = pattern(70_000, 41);
    let old_path = source("arbitrary-kill-old", &old);
    ok(tool("fs.xfs").args([
        base.path().to_str().unwrap(),
        "import",
        old_path.to_str().unwrap(),
        "/victim.bin",
    ]));
    let new = pattern(96 << 20, 42);
    let new_path = source("arbitrary-kill-new", &new);
    let old_sum = sha256_hex(&old);
    let new_sum = sha256_hex(&new);
    let args = [
        "import",
        "--replace",
        "--verify",
        new_path.to_str().unwrap(),
        "/victim.bin",
    ];
    let window = uninterrupted(base.path(), "arbitrary-kill", &args);
    kill_loop(
        base.path(),
        "arbitrary-kill",
        &args,
        window,
        ATTEMPTS,
        &["/victim.bin"],
        |attempt, graded| {
            let actual = graded.sum("/victim.bin");
            assert!(
                actual == old_sum || actual == new_sum,
                "attempt {attempt}: the kernel read a partial replacement, digest {actual}"
            );
        },
    );
}

#[test]
fn arbitrary_kills_during_write_leave_a_replayable_volume() {
    const ATTEMPTS: usize = 100;
    let base = fixture("xfscli-v5.img");
    let data = pattern(64 << 20, 43);
    let data_path = source("arbitrary-write", &data);
    // `fs.xfs write` is not atomic: a kill may leave no file, an empty one
    // or one partly written. What is checked is the volume, not the file.
    let args = ["write", "/written.bin", "<", data_path.to_str().unwrap()];
    let window = uninterrupted(&base, "arbitrary-write", &args);
    kill_loop(
        &base,
        "arbitrary-write",
        &args,
        window,
        ATTEMPTS,
        &[],
        |_, _| {},
    );
}

#[test]
fn import_preserving_links_and_times_is_what_the_kernel_reads() {
    let volume = copy(&fixture("xfscli-v5.img"), "links");
    let image = volume.path().to_str().unwrap();
    let tree = scratch::dir(SUITE).join(format!("{}-links", std::process::id()));
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(tree.join("d")).unwrap();
    std::fs::write(tree.join("f1"), b"data\n").unwrap();
    std::fs::hard_link(tree.join("f1"), tree.join("d/f2")).unwrap();
    std::fs::hard_link(tree.join("f1"), tree.join("f3")).unwrap();
    std::os::unix::fs::symlink("f1", tree.join("rel")).unwrap();
    std::os::unix::fs::symlink("/abs/target", tree.join("abs")).unwrap();
    for path in ["f1", "d"] {
        // touch -t reads local time; UTC pins the expected epoch on every host.
        ok(std::process::Command::new("touch").env("TZ", "UTC").args([
            "-t",
            "202001020304.05",
            tree.join(path).to_str().unwrap(),
        ]));
    }
    let out = ok(tool("fs.xfs").args([
        image,
        "import",
        "--recursive",
        "--preserve=times,links",
        tree.to_str().unwrap(),
        "/t",
    ]));
    assert_eq!(json_field(&stdout(&out), "failed"), "0", "{}", stdout(&out));
    assert_eq!(
        json_field(&stdout(&out), "refused"),
        "0",
        "{}",
        stdout(&out)
    );

    let out = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m" && echo MOUNT_OK
        for f in t/f1 t/d/f2 t/f3; do echo "LINKS $(stat -c '%h' "$work/m/$f") $(stat -c '%i' "$work/m/$f")"; done
        echo "REL $(readlink "$work/m/t/rel")"
        echo "ABS $(readlink "$work/m/t/abs")"
        for f in t/f1 t/d; do echo "TIMES $(stat -c '%Y %X' "$work/m/$f")"; done
        umount "$work/m" || echo UMOUNT_FAILED
        {repair}
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$work/image\""),
    ));
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount it:\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    repair::assert_agreed(&out, "the volume import --preserve=times,links left");
    let links: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("LINKS "))
        .collect();
    assert_eq!(links.len(), 3, "{out}");
    let inode = |l: &str| l.split(' ').nth(1).unwrap().to_string();
    assert!(
        links.iter().all(|l| l.starts_with("3 "))
            && links.iter().all(|l| inode(l) == inode(links[0])),
        "the three names are not one inode of link count 3: {links:?}"
    );
    assert!(out.lines().any(|l| l == "REL f1"), "{out}");
    assert!(out.lines().any(|l| l == "ABS /abs/target"), "{out}");
    let times: Vec<&str> = out
        .lines()
        .filter_map(|l| l.strip_prefix("TIMES "))
        .collect();
    assert_eq!(times, ["1577934245 1577934245"; 2], "{out}");
    let _ = std::fs::remove_dir_all(&tree);
}
