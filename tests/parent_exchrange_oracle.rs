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
//! Needs xfsprogs 6.10 or newer, the first with parent pointers.
//! `XFSPROGS_PARENT_BIN` names a directory holding such a `mkfs.xfs`,
//! `xfs_db` and `xfs_repair`; without it they come from `PATH`. CI builds
//! them, because Ubuntu's are older. `#[ignore]`-gated like the other
//! inline xfsprogs oracles.

// Only the repair check is wanted here — this oracle runs the tools on
// this machine rather than through a guest — and a module included whole
// is a module whose other helpers are unused in this binary.
#[allow(dead_code)]
mod common;

use common::repair;
use fs_core::FileDevice;
use fs_xfs::superblock::incompat;
use fs_xfs::{Error, Filesystem};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

fn tool(name: &str) -> PathBuf {
    let path = match std::env::var_os("XFSPROGS_PARENT_BIN") {
        Some(dir) => Path::new(&dir).join(name),
        None => PathBuf::from(name),
    };
    let out = Command::new(&path)
        .arg("-V")
        .output()
        .unwrap_or_else(|e| panic!("{}: {e}; install xfsprogs 6.10 or newer", path.display()));
    let version = String::from_utf8_lossy(&out.stdout);
    let (major, minor) = version
        .rsplit(' ')
        .next()
        .and_then(|v| {
            let mut parts = v.trim().split('.').map(|p| p.parse::<u32>().ok());
            Some((parts.next()??, parts.next()??))
        })
        .unwrap_or_else(|| panic!("{}: unreadable version {version:?}", path.display()));
    assert!(
        (major, minor) >= (6, 10),
        "{} is {major}.{minor}; parent pointers need xfsprogs 6.10 or newer \
         (set XFSPROGS_PARENT_BIN to a directory holding one)",
        path.display()
    );
    path
}

fn run(cmd: &mut Command) -> String {
    let out = cmd.output().expect("run xfsprogs");
    assert!(
        out.status.success(),
        "{cmd:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// A volume made by `mkfs.xfs $args` from a protofile:
/// - `/dir/hello.txt`, with 30 user attributes (a leaf-form attribute fork);
/// - `/top.txt`, with one (short form);
/// - `/dir/link`, a symlink;
/// - `/many/`, with 200 entries, which is past block form.
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
    run(Command::new(tool("mkfs.xfs"))
        .args(["-q", "-f"])
        .args(args)
        .arg("-p")
        .arg(&proto_path)
        .arg(&img));

    let mut db = Command::new(tool("xfs_db"));
    db.args(["-x", "-c", "path /dir/hello.txt"]);
    for i in 0..30 {
        db.args(["-c", &format!("attr_set -u name_{i:02} value_{i:02}")]);
    }
    db.args(["-c", "path /top.txt", "-c", "attr_set -u colour red"]);
    run(db.arg(&img));
    repair::assert_agreed_running(
        tool("xfs_repair").to_str().expect("a path"),
        img.to_str().expect("a path"),
        "the fixture this oracle grades against",
    );
    (dir, big_bytes)
}

/// How many attribute entries xfs_db sees on `path`, and how many of them
/// are parent pointers.
fn xfs_db_attr_counts(img: &Path, path: &str) -> (usize, usize) {
    let out = run(Command::new(tool("xfs_db"))
        .args([
            "-r",
            "-c",
            &format!("path {path}"),
            "-c",
            "print core.aformat",
            "-c",
            "print a",
        ])
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
    let leaf = run(Command::new(tool("xfs_db"))
        .args([
            "-r",
            "-c",
            &format!("path {path}"),
            "-c",
            "ablock 0",
            "-c",
            "print",
        ])
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
#[ignore = "needs xfsprogs 6.10 or newer"]
fn a_parent_pointer_volume_reads_and_refuses_writes() {
    check_reads(
        "parent",
        &["-n", "parent=1"],
        incompat::PARENT | incompat::EXCHRANGE,
    );
}

#[test]
#[ignore = "needs xfsprogs 6.10 or newer"]
fn an_exchange_range_volume_reads_and_refuses_writes() {
    check_reads("exchange", &["-i", "exchange=1"], incompat::EXCHRANGE);
}
