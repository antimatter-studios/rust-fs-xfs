//! Volumes with parent pointers or exchange-range read correctly (#99),
//! and are written (#373, #374).
//!
//! `mkfs.xfs -n parent=1` sets both incompat bits: parent pointers (7) and
//! exchange-range (6), which parent pointers need. `-i exchange=1` sets
//! exchange-range alone. Neither changes a structure a read walks.
//! - Parent pointers are extended attributes in their own namespace, one
//!   per link, beside the user's attributes in the same attribute fork.
//! - Exchange-range adds log items, and a volume with a dirty log is
//!   refused before anything reads it.
//!
//! So a read accepts both bits. A write accepts exchange-range (#374),
//! whose exchanges are graded on the Debian 13 guest by
//! `tests/exchange_range_kernel.rs`. It accepts parent pointers too (#373):
//! every create, link, rename and unlink adds, moves or removes the pointer
//! its change implies. The driver makes each kind of change here, unmounts,
//! and the pinned `xfs_repair -n`, which checks every parent pointer
//! against the directory entries, grades the volume as it lies.
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
use fs_xfs::attr::ParentPointer;
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

/// What a writable mount of the volume is asked to do: keep its parent
/// pointers through every kind of namespace change (#373), or only mount,
/// as an exchange-range volume does here (#374).
#[derive(PartialEq)]
enum Writes {
    KeepParents,
    Accepted,
}

fn check_reads(tag: &str, args: &[&str], bits: u32, writes: Writes) {
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

    // Each inode's parent pointers are exactly the directory entries that
    // name it, each with its directory's generation, and a volume without
    // the feature has none (#373). The entries are the directory's own,
    // read by a walk that never looks at an attribute fork, so the two
    // agree only if the parent pointers are read right.
    for (path, name, parent_path) in [
        ("/dir", "dir", "/"),
        ("/dir/hello.txt", "hello.txt", "/dir"),
        ("/dir/link", "link", "/dir"),
        ("/top.txt", "top.txt", "/"),
        ("/many", "many", "/"),
        ("/many/entry_137", "entry_137", "/many"),
    ] {
        let file = fs.open(path).unwrap();
        let parent = fs.open(parent_path).unwrap();
        let got = fs
            .parent_pointers(file.inode(), file.raw())
            .unwrap_or_else(|e| panic!("{tag} {path}: {e:?}"));
        let want = if bits & incompat::PARENT != 0 {
            vec![ParentPointer {
                name: name.as_bytes().to_vec(),
                parent: parent.inode().ino,
                generation: parent.inode().gen,
            }]
        } else {
            Vec::new()
        };
        assert_eq!(got, want, "{tag} {path}: its parent pointers");
    }

    drop(fs);
    if writes == Writes::KeepParents {
        keep_parent_pointers(tag, &img);
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }

    // A writable mount is accepted (#374). What an exchange on this volume
    // leaves behind is graded where a kernel can replay the log first, on
    // the Debian 13 guest: tests/exchange_range_kernel.rs. This guest's
    // 6.1 kernel cannot mount an exchange-range volume at all, and
    // xfs_repair will not grade a log nothing has replayed.
    let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&img).unwrap()))
        .unwrap_or_else(|e| panic!("{tag}: a writable mount must succeed: {e:?}"));
    drop(fs);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each kind of namespace change this driver makes, on the volume `build`
/// made, after which the pinned `xfs_repair -n` must call it clean and
/// every name must have exactly the parent pointers its links imply.
///
/// - `create_directory`, `create_file` and `create_symlink` give the new
///   inode its first pointer.
/// - `link` adds one; `unlink_file` of one of two links removes one.
/// - `rename` between directories moves a file's and a directory's, and
///   replacing a name another link keeps alive removes the victim's.
/// - `rename_in_directory`, and `rename` within one directory, rename one.
/// - `unlink_file` of a last link and `remove_directory` free the inode.
///
/// `/dir/hello.txt` holds 30 user attributes, a leaf-form fork, where a
/// parent pointer is not written yet: a link to it is refused, and the
/// volume `xfs_repair` grades afterwards shows the refusal changed nothing.
fn keep_parent_pointers(tag: &str, img: &Path) {
    let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(img).unwrap()))
        .unwrap_or_else(|e| panic!("{tag}: a writable mount must succeed: {e:?}"));
    let root = fs.superblock().rootino;
    let ino = |path: &str| {
        fs.lookup_path(path)
            .unwrap_or_else(|e| panic!("{path}: {e:?}"))
            .ino
    };

    let (made, _) = fs.create_directory(root, b"made", 0o040755).expect("mkdir");
    fs.create_file(made, b"new.txt", 0o100644).expect("create");
    fs.create_symlink(made, b"sym", b"../top.txt")
        .expect("symlink");
    fs.link(ino("/top.txt"), made, b"top-link").expect("link");
    assert!(
        matches!(
            fs.link(ino("/dir/hello.txt"), made, b"hello-link"),
            Err(Error::UnsupportedFeature(_))
        ),
        "{tag}: a link to an inode whose attributes are past short form must be refused"
    );
    // A rename trades the moved inode's pointer for its new name's, so the
    // same fork refuses it, and neither name changes.
    assert!(
        matches!(
            fs.rename(ino("/dir"), b"hello.txt", made, b"hello-moved"),
            Err(Error::UnsupportedFeature(_))
        ),
        "{tag}: a rename of an inode whose attributes are past short form must be refused"
    );
    fs.rename(root, b"top.txt", made, b"top-moved.txt")
        .expect("move a file with two links");
    fs.unlink_file(made, b"top-link")
        .expect("unlink one of two links");
    let many = ino("/many");
    fs.rename_in_directory(many, b"entry_000", b"entry_first")
        .expect("rename in a directory");
    fs.unlink_file(many, b"entry_001")
        .expect("unlink a last link");
    let (victim, _) = fs.create_file(made, b"victim", 0o100644).expect("create");
    fs.link(victim, root, b"victim-too")
        .expect("link the victim");
    fs.rename(many, b"entry_002", made, b"victim")
        .expect("replace a name another link keeps alive");
    fs.rename(root, b"dir", made, b"dir-moved")
        .expect("move a directory");
    fs.rename(made, b"new.txt", made, b"renamed.txt")
        .expect("rename within one directory");
    let (empty, _) = fs
        .create_directory(made, b"empty", 0o040755)
        .expect("mkdir");
    fs.create_file(empty, b"gone", 0o100644).expect("create");
    fs.unlink_file(empty, b"gone").expect("unlink");
    fs.remove_directory(made, b"empty").expect("rmdir");
    let (twice, _) = fs.create_file(made, b"twice", 0o100644).expect("create");
    fs.link(twice, many, b"twice-in-many").expect("link");
    fs.unmount().expect("unmount");

    // No mount first: the unmount left a clean log (#439), so the pinned
    // tool grades the volume as the driver left it.
    let report = parent_oracle("xfs_repair").arg("-n").arg(img).output();
    let said = report.repair_report();
    repair::assert_agreed(&said, &format!("{tag}: the volume after namespace changes"));
    // `-n` reports what it would change, parent pointers included, and a
    // clean volume has nothing it would change.
    assert!(
        !said.to_ascii_lowercase().contains("would"),
        "{tag}: xfs_repair would change the volume:\n{said}"
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open(img).unwrap())).unwrap();
    for (path, links) in [
        ("/made", &[("made", "/")][..]),
        ("/made/renamed.txt", &[("renamed.txt", "/made")]),
        ("/made/sym", &[("sym", "/made")]),
        ("/made/top-moved.txt", &[("top-moved.txt", "/made")]),
        ("/many/entry_first", &[("entry_first", "/many")]),
        ("/victim-too", &[("victim-too", "/")]),
        ("/made/victim", &[("victim", "/made")]),
        ("/made/dir-moved", &[("dir-moved", "/made")]),
        (
            "/made/dir-moved/hello.txt",
            &[("hello.txt", "/made/dir-moved")],
        ),
        ("/made/dir-moved/link", &[("link", "/made/dir-moved")]),
        (
            "/made/twice",
            &[("twice", "/made"), ("twice-in-many", "/many")],
        ),
    ] {
        let file = fs
            .open(path)
            .unwrap_or_else(|e| panic!("{tag} {path}: {e:?}"));
        let mut got = fs.parent_pointers(file.inode(), file.raw()).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let want: Vec<ParentPointer> = links
            .iter()
            .map(|(name, parent)| {
                let parent = fs.open(parent).unwrap();
                ParentPointer {
                    name: name.as_bytes().to_vec(),
                    parent: parent.inode().ino,
                    generation: parent.inode().gen,
                }
            })
            .collect();
        assert_eq!(
            got, want,
            "{tag} {path}: its parent pointers after the writes"
        );
    }
    // The independent count of the same thing: xfs_db, reading the fork.
    for (path, parents) in [("/made/top-moved.txt", 1), ("/made/twice", 2)] {
        assert_eq!(
            xfs_db_attr_counts(img, path).1,
            parents,
            "{tag} {path}: xfs_db's count of its parent pointers"
        );
    }
    for gone in [
        "/many/entry_000",
        "/many/entry_001",
        "/many/entry_002",
        "/made/empty",
        "/made/hello-link",
        "/made/hello-moved",
    ] {
        assert!(
            fs.lookup_path(gone).is_err(),
            "{tag}: {gone} is still there"
        );
    }
}

#[test]
fn a_parent_pointer_volume_reads_and_keeps_its_parent_pointers_through_writes() {
    check_reads(
        "parent",
        &["-n", "parent=1"],
        incompat::PARENT | incompat::EXCHRANGE,
        Writes::KeepParents,
    );
}

#[test]
fn an_exchange_range_volume_reads_and_accepts_a_writable_mount() {
    check_reads(
        "exchange",
        &["-i", "exchange=1"],
        incompat::EXCHRANGE,
        Writes::Accepted,
    );
}
