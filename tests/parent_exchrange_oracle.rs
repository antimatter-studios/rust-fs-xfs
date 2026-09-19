//! Volumes with parent pointers or exchange-range read correctly, and are
//! refused for writing (#99).
//!
//! `mkfs.xfs -n parent=1` sets both incompat bits: parent pointers (7) and
//! exchange-range (6), which parent pointers need. `-i exchange=1` sets
//! exchange-range alone. Neither changes a structure a read walks.
//! - Parent pointers are extended attributes in their own namespace, one
//!   per link, beside the user's attributes in the same attribute fork.
//! - Exchange-range adds log items, and a volume with a dirty log is
//!   refused before anything reads it.
//!
//! So a read accepts both bits. A write does not: every create, rename and
//! unlink must maintain the parent pointers, and nothing here does.
//!
//! The volumes are built without a kernel. A protofile populates them, so
//! mkfs writes the parent pointers, and `xfs_db -x` adds user attributes
//! beside them in short form and in leaf form. `xfs_repair -n` must accept
//! each volume as built, and xfs_db counts the attribute entries the driver
//! must report.
//!
//! Parent pointers arrived in xfsprogs 6.10 and Debian's is 6.1, so these
//! tools are not the guest's ordinary ones: `scripts/vm-setup.sh` builds a
//! newer xfsprogs beside them and `common::parent_oracle` names it. That
//! build is pinned there, once, for every host this suite runs on, which
//! is why nothing here asks a tool its version — a test that probes for a
//! capability is a test that can decide it has nothing to do, and this one
//! covers a format the driver either reads or does not.

mod common;

use common::{parent_oracle, repair, Oracle};
use fs_core::FileDevice;
use fs_xfs::superblock::incompat;
use fs_xfs::{Error, Filesystem};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Run one of the pinned tools in the guest, and return its stdout. A
/// tool that fails here has been handed a volume it could not build or
/// could not read, which is the finding, so both streams come with it.
fn run(call: Oracle) -> String {
    let out = call.output();
    assert!(out.ok(), "xfsprogs failed:\n{}{}", out.stdout, out.stderr);
    out.stdout
}

/// A volume made by `mkfs.xfs $args` from a protofile:
/// - `/dir/hello.txt`, with 30 user attributes (a leaf-form attribute fork);
/// - `/top.txt`, with one (short form);
/// - `/dir/link`, a symlink;
/// - `/many/`, with 200 entries, which is past block form.
///
/// The protofile and the source files it names are handed to mkfs.xfs as
/// paths, and mkfs.xfs reads them in the guest, so they have to be
/// somewhere the guest has: `std::env::temp_dir()` is that, because
/// `scripts/with-test-temp.sh` points TMPDIR at a directory inside this
/// repository and the harness mounts the repository in the guest at the
/// path the host knows it by.
fn build(tag: &str, args: &[&str]) -> (PathBuf, Vec<u8>) {
    let dir = std::env::temp_dir().join(format!("fs-xfs-99-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hello = dir.join("hello.src");
    std::fs::write(&hello, b"hello, parent pointers\n").unwrap();
    let big = dir.join("big.src");
    let big_bytes: Vec<u8> = (0..70_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&big, &big_bytes).unwrap();

    let mut proto = format!(
        "/dummy\n0 0\nd--755 0 0\n\
         dir d--755 0 0\n\
         hello.txt ---644 0 0 {hello}\n\
         link l--777 0 0 hello.txt\n\
         $\n\
         top.txt ---644 0 0 {big}\n\
         many d--755 0 0\n",
        hello = hello.display(),
        big = big.display()
    );
    for i in 0..200 {
        proto.push_str(&format!("entry_{i:03} ---644 0 0 {}\n", hello.display()));
    }
    proto.push_str("$\n$\n");
    let proto_path = dir.join("proto");
    std::fs::write(&proto_path, proto).unwrap();

    let img = dir.join("fs.img");
    std::fs::File::create(&img)
        .unwrap()
        .set_len(300 * 1024 * 1024)
        .unwrap();
    run(parent_oracle("mkfs.xfs")
        .args(["-q", "-f"])
        .args(args)
        .arg("-p")
        .arg(&proto_path)
        .arg(&img));

    let mut db = parent_oracle("xfs_db").args(["-x", "-c", "path /dir/hello.txt"]);
    for i in 0..30 {
        db = db
            .arg("-c")
            .arg(format!("attr_set -u name_{i:02} value_{i:02}"));
    }
    db = db.args(["-c", "path /top.txt", "-c", "attr_set -u colour red"]);
    run(db.arg(&img));
    // The pinned build, in the guest, and its report read as a verdict:
    // a clean exit beside "valuable metadata changes in a log" is the
    // tool declining to look, not this volume being sound (#124).
    let out = parent_oracle("xfs_repair").arg("-n").arg(&img).output();
    repair::assert_agreed(
        &out.repair_report(),
        "the fixture this oracle grades against",
    );
    (dir, big_bytes)
}

/// How many attribute entries xfs_db sees on `path`, and how many of them
/// are parent pointers.
fn xfs_db_attr_counts(img: &Path, path: &str) -> (usize, usize) {
    let out = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "print core.aformat", "-c", "print a"])
        .arg(img));
    if out.contains("core.aformat = 1 (local)") {
        let count = out
            .lines()
            .find_map(|l| l.strip_prefix("a.sfattr.hdr.count = "))
            .expect("a short-form count")
            .trim()
            .parse()
            .unwrap();
        let parents = out.lines().filter(|l| l.ends_with(".parent = 1")).count();
        return (count, parents);
    }
    // A leaf: one block, as 32 small entries are.
    let leaf = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "ablock 0", "-c", "print"])
        .arg(img));
    let count = leaf
        .lines()
        .find_map(|l| l.strip_prefix("hdr.count = "))
        .expect("a leaf count")
        .trim()
        .parse()
        .unwrap();
    // Entry flags print as one tuple per entry here, but only a parent
    // pointer's value decodes to a directory.
    let parents = leaf
        .lines()
        .filter(|l| l.contains(".parent_dir.inumber = "))
        .count();
    (count, parents)
}

fn check_reads(tag: &str, args: &[&str], bits: u32) {
    let (dir, big_bytes) = build(tag, args);
    let img = dir.join("fs.img");
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).unwrap()))
        .unwrap_or_else(|e| panic!("{tag}: a read-only mount must succeed: {e:?}"));
    assert_eq!(
        fs.superblock().features_incompat & (incompat::PARENT | incompat::EXCHRANGE),
        bits,
        "{tag}: mkfs did not set the bits this case is about"
    );

    assert_eq!(
        fs.read_path("/dir/hello.txt").unwrap(),
        b"hello, parent pointers\n",
        "{tag}"
    );
    assert_eq!(fs.read_path("/top.txt").unwrap(), big_bytes, "{tag}");
    assert_eq!(
        fs.open("/dir/link").unwrap().link_target().unwrap(),
        b"hello.txt",
        "{tag}"
    );
    let mut names: Vec<Vec<u8>> = fs
        .list_path("/many")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .filter(|n| n != b"." && n != b"..")
        .collect();
    names.sort();
    let expected: Vec<Vec<u8>> = (0..200)
        .map(|i| format!("entry_{i:03}").into_bytes())
        .collect();
    assert_eq!(names, expected, "{tag}: /many");
    assert_eq!(
        fs.read_path("/many/entry_137").unwrap(),
        b"hello, parent pointers\n",
        "{tag}"
    );

    for (path, set) in [
        (
            "/dir/hello.txt",
            (0..30)
                .map(|i| (format!("user.name_{i:02}"), format!("value_{i:02}")))
                .collect::<Vec<_>>(),
        ),
        ("/top.txt", vec![("user.colour".into(), "red".into())]),
    ] {
        let file = fs.open(path).unwrap();
        let xattrs = fs.list_xattrs(file.inode(), file.raw()).unwrap();
        let (on_disk, parents) = xfs_db_attr_counts(&img, path);
        let expected_parents = usize::from(bits & incompat::PARENT != 0);
        assert_eq!(
            parents, expected_parents,
            "{tag} {path}: xfs_db sees {parents} parent pointers"
        );
        assert_eq!(
            xattrs.len(),
            on_disk - parents,
            "{tag} {path}: every attribute but the parent pointers is reported: {:?}",
            xattrs
                .iter()
                .map(|x| String::from_utf8_lossy(&x.name).into_owned())
                .collect::<Vec<_>>()
        );
        for (name, value) in set {
            assert_eq!(
                fs.get_xattr(file.inode(), file.raw(), name.as_bytes())
                    .unwrap()
                    .as_deref(),
                Some(value.as_bytes()),
                "{tag} {path}: {name}"
            );
        }
    }

    drop(fs);
    match Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&img).unwrap())) {
        Err(Error::UnsupportedFeature(why)) => assert!(
            why.contains("can be read but not written"),
            "{tag}: the refusal should say the volume stays readable: {why}"
        ),
        Err(other) => panic!("{tag}: wrong refusal {other:?}"),
        Ok(_) => panic!("{tag}: a writable mount must be refused"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_parent_pointer_volume_reads_and_refuses_writes() {
    check_reads(
        "parent",
        &["-n", "parent=1"],
        incompat::PARENT | incompat::EXCHRANGE,
    );
}

#[test]
fn an_exchange_range_volume_reads_and_refuses_writes() {
    check_reads("exchange", &["-i", "exchange=1"], incompat::EXCHRANGE);
}
