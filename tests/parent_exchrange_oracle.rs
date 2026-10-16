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
//! beside them in short, leaf and node form. `xfs_repair -n` must accept
//! each volume as built, xfs_db counts the attribute entries the driver
//! must report, and xfs_db lists the parent pointers in every form, which
//! the driver's own reading has to match (#488): its `parent` command for a
//! leaf or node fork, and a short-form fork's own entries otherwise (#513).
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

/// The SHA-256 of every byte of `img`, streamed from the file, so a
/// refusal can be shown to have written nothing without a second copy.
fn image_digest(img: &Path) -> Vec<u8> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(img).unwrap();
    std::io::copy(&mut file, &mut hasher).unwrap();
    hasher.finalize().to_vec()
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

/// Every parent pointer xfs_db lists for `path`, as (directory, its
/// generation, name), sorted: a reading of an inode's pointers that is not
/// the driver's, whatever form its attribute fork is in (#488).
///
/// A leaf or node fork is listed by xfs_db's `parent` command, which walks
/// a leaf and the whole leaf chain of a node fork. It prints one
/// `%18llu:0x%08x %3d %.*s` line per pointer, and a line that does not
/// parse as one fails the test rather than being dropped.
///
/// A short-form fork is not, because `parent` misreads it (#513). The
/// pinned xfsprogs 6.13's `list_sf_pptrs`, in db/namei.c, hands each entry
/// on with its value at `sfe->nameval + sfe->valuelen`, where the value
/// starts at `sfe->nameval + sfe->namelen`. Every pointer whose name is not
/// 12 bytes long then decodes to a record the tool rejects and drops
/// without a word, whoever wrote the fork, and the command still exits 0.
/// `print a` decodes the same entries at the right offset (db/attrshort.c,
/// where libxfs reads them), so a short-form fork is read from its fields.
fn xfs_db_parents(img: &Path, path: &str) -> Vec<(u64, u32, String)> {
    let fork = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "print core.aformat", "-c", "print a"])
        .arg(img));
    if fork.contains("core.aformat = 1 (local)") {
        return xfs_db_short_form_parents(path, &fork);
    }
    let out = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "parent"])
        .arg(img));
    let mut got: Vec<(u64, u32, String)> = out
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let parsed = || -> Option<(u64, u32, String)> {
                let (ino, rest) = line.trim_start().split_once(":0x")?;
                let (generation, rest) = rest.split_once(' ')?;
                let (len, name) = rest.trim_start().split_once(' ')?;
                let len: usize = len.parse().ok()?;
                if name.len() != len {
                    return None;
                }
                Some((
                    ino.parse().ok()?,
                    u32::from_str_radix(generation, 16).ok()?,
                    name.to_string(),
                ))
            };
            parsed().unwrap_or_else(|| {
                panic!(
                    "{path}: xfs_db printed a line that is not a parent pointer: {line:?}\n{out}"
                )
            })
        })
        .collect();
    got.sort();
    got
}

/// The parent pointers in the short-form fork `print a` printed as `out`,
/// sorted. Each entry prints as `a.sfattr.list[i].<field> = <value>` lines;
/// a parent pointer is one whose `parent` flag is 1, and its `name`,
/// `parent_dir.inumber` and `parent_dir.gen` are the pointer. Every entry
/// the header counts must be printed, and every parent pointer must carry
/// a 12-byte value, as `parent` itself requires, and a name as long as its
/// `namelen`; anything else fails the test rather than being dropped.
fn xfs_db_short_form_parents(path: &str, out: &str) -> Vec<(u64, u32, String)> {
    #[derive(Default)]
    struct Entry {
        namelen: Option<usize>,
        valuelen: Option<usize>,
        parent: Option<bool>,
        name: Option<Vec<u8>>,
        ino: Option<u64>,
        generation: Option<u32>,
    }
    let count: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("a.sfattr.hdr.count = "))
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or_else(|| panic!("{path}: xfs_db printed no short-form entry count:\n{out}"));
    let mut entries = std::collections::BTreeMap::<usize, Entry>::new();
    for line in out.lines() {
        let Some(rest) = line.strip_prefix("a.sfattr.list[") else {
            continue;
        };
        let mut parsed = || -> Option<()> {
            let (index, rest) = rest.split_once("].")?;
            let (field, value) = rest.split_once(" = ")?;
            let entry = entries.entry(index.parse().ok()?).or_default();
            match field {
                "namelen" => entry.namelen = Some(value.parse().ok()?),
                "valuelen" => entry.valuelen = Some(value.parse().ok()?),
                "parent" => entry.parent = Some(value.parse::<u8>().ok()? == 1),
                "name" => entry.name = Some(xfs_db_unquote(value)?),
                "parent_dir.inumber" => entry.ino = Some(value.parse().ok()?),
                "parent_dir.gen" => entry.generation = Some(value.parse().ok()?),
                _ => {}
            }
            Some(())
        };
        parsed().unwrap_or_else(|| {
            panic!("{path}: xfs_db printed a short-form entry line it cannot be read from: {line:?}\n{out}")
        });
    }
    assert!(
        entries.keys().copied().eq(0..count),
        "{path}: xfs_db printed entries {:?} of a short-form fork whose header counts {count}:\n{out}",
        entries.keys().collect::<Vec<_>>()
    );
    let mut got: Vec<(u64, u32, String)> = entries
        .into_iter()
        .filter(|(_, entry)| entry.parent != Some(false))
        .map(|(index, entry)| {
            let pointer = || -> Option<(u64, u32, String)> {
                let name = entry.name?;
                if entry.parent? && entry.valuelen? == 12 && name.len() == entry.namelen? {
                    Some((entry.ino?, entry.generation?, String::from_utf8(name).ok()?))
                } else {
                    None
                }
            };
            pointer().unwrap_or_else(|| {
                panic!("{path}: short-form entry {index} as xfs_db printed it is not a parent pointer:\n{out}")
            })
        })
        .collect();
    got.sort();
    got
}

/// A name as xfs_db's `charns` printer writes it: in double quotes, with
/// `\`, `'`, `"` and `?` escaped by a backslash, a control byte from 7 to
/// 13 as a backslash and the letter that byte plus 90 is (`\a` to `\g`),
/// and any other byte that is not printable as `\` and three octal digits.
fn xfs_db_unquote(printed: &str) -> Option<Vec<u8>> {
    let mut bytes = printed.strip_prefix('"')?.strip_suffix('"')?.bytes();
    let mut name = Vec::new();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            name.push(byte);
            continue;
        }
        let escaped = bytes.next()?;
        name.push(match escaped {
            b'\\' | b'\'' | b'"' | b'?' => escaped,
            b'a'..=b'g' => escaped - 90,
            b'0'..=b'7' => {
                let digits = [escaped, bytes.next()?, bytes.next()?];
                u8::from_str_radix(std::str::from_utf8(&digits).ok()?, 8).ok()?
            }
            _ => return None,
        });
    }
    Some(name)
}

/// The form of `path`'s attribute fork as xfs_db reads it: `local`, or
/// past short form `leaf` when block 0 is a leaf and `node` when it is a
/// node, the only attribute block with a level.
fn xfs_db_attr_form(img: &Path, path: &str) -> &'static str {
    let core = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "print core.aformat"])
        .arg(img));
    if core.contains("core.aformat = 1 (local)") {
        return "local";
    }
    let block = run(parent_oracle("xfs_db")
        .args(["-r", "-c"])
        .arg(format!("path {path}"))
        .args(["-c", "ablock 0", "-c", "print"])
        .arg(img));
    if block.lines().any(|l| l.starts_with("hdr.level = ")) {
        "node"
    } else if block.lines().any(|l| l.starts_with("hdr.count = ")) {
        "leaf"
    } else {
        panic!(
            "{path}: xfs_db reads attribute block 0 as neither a leaf nor a node:\n{core}{block}"
        )
    }
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
/// The first three kinds are made again on inodes whose attribute fork is
/// past short form (#488), where adding or removing a pointer rebuilds the
/// fork's blocks: `/dir/hello.txt` with 30 user attributes and `/multi`
/// with 30 more given by xfs_db, both leaf form; `/bulky`, given 300, more
/// than one leaf holds, so node form; one rename replacing a name of the
/// node-form inode with the leaf-form one, both forks rebuilt in one
/// record; and a file whose second long name does not fit short form, so
/// the link itself moves its fork to a leaf. Their user attributes have to
/// come through every rebuild, and xfs_db's own listing of each one's
/// parent pointers has to match its names.
///
/// The last name of an inode whose attribute fork has blocks of its own
/// frees every one of them (#495): `/multi`, a leaf, once its other name is
/// gone; `/treed`, whose remote values xfs_db mapped with holes between
/// them until the map became a B+tree; and `/noded`, a node, replaced by a
/// rename. The pinned `xfs_repair -n` checks that each block is free again
/// and owned by nothing.
///
/// Still refused before anything is written, with the image byte-for-byte
/// what it was and the volume `xfs_repair` grades afterwards agreeing:
/// linking `/remote`, whose fork past short form holds a 3000-byte value
/// in blocks of its own (#501).
fn keep_parent_pointers(tag: &str, img: &Path) {
    // `/multi` and `/multi-too` are two links to one inode, and `/bulky`
    // and `/bulky-too` two to another. The driver makes them while their
    // forks are short form; xfs_db then gives `/multi` 30 user attributes,
    // a leaf, and `/bulky` 300, a node.
    {
        let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(img).unwrap()))
            .unwrap_or_else(|e| panic!("{tag}: a writable mount must succeed: {e:?}"));
        let root = fs.superblock().rootino;
        for (first, second) in [
            ("multi", "multi-too"),
            ("bulky", "bulky-too"),
            ("remote", "remote-too"),
        ] {
            let (made, _) = fs
                .create_file(root, first.as_bytes(), 0o100644)
                .expect("create");
            fs.link(made, root, second.as_bytes()).expect("link");
        }
        // One name each, so removing it frees the inode (#495).
        for only in ["noded", "treed"] {
            fs.create_file(root, only.as_bytes(), 0o100644)
                .expect("create");
        }
        fs.unmount().expect("unmount");
    }
    for (path, count) in [("/multi", 30), ("/bulky", 300), ("/noded", 300)] {
        let mut db = parent_oracle("xfs_db")
            .args(["-x", "-c"])
            .arg(format!("path {path}"));
        for i in 0..count {
            db = db
                .arg("-c")
                .arg(format!("attr_set -u name_{i:03} value_{i:03}"));
        }
        run(db.arg(img));
    }
    // A value past what a leaf keeps beside its name, so stored remotely.
    run(parent_oracle("xfs_db")
        .args(["-x", "-c", "path /remote", "-c", "attr_set -u -v 3000 big"])
        .arg(img));
    // Sixty remote values, then every other one removed: each leaves a hole
    // in the fork's map, the thirty extents left do not fit the inode, and
    // the map becomes a B+tree (#495).
    let mut db = parent_oracle("xfs_db").args(["-x", "-c", "path /treed"]);
    for i in 0..60 {
        db = db.arg("-c").arg(format!("attr_set -u -v 3000 big_{i:02}"));
    }
    for i in (1..60).step_by(2) {
        db = db.arg("-c").arg(format!("attr_remove -u big_{i:02}"));
    }
    run(db.arg(img));
    for (path, form) in [
        ("/dir/hello.txt", "leaf"),
        ("/multi", "leaf"),
        ("/bulky", "node"),
        ("/remote", "leaf"),
        ("/noded", "node"),
    ] {
        assert_eq!(
            xfs_db_attr_form(img, path),
            form,
            "{tag} {path}: the attribute fork xfs_db built"
        );
    }
    let treed = run(parent_oracle("xfs_db")
        .args(["-r", "-c", "path /treed", "-c", "print core.aformat"])
        .arg(img));
    assert!(
        treed.contains("core.aformat = 3 (btree)"),
        "{tag} /treed: xfs_db did not map its attribute fork with a B+tree:\n{treed}"
    );
    for path in ["/multi", "/bulky", "/remote"] {
        assert_eq!(
            xfs_db_parents(img, path).len(),
            2,
            "{tag} {path}: xfs_db's parent pointers before the changes"
        );
    }

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

    // PAST SHORT FORM (#488). A leaf-form fork gains a pointer, has one
    // moved to another directory, and loses one to a replacing rename.
    fs.link(ino("/dir/hello.txt"), made, b"hello-link")
        .expect("link an inode whose attribute fork is a leaf");
    fs.rename(ino("/dir"), b"hello.txt", made, b"hello-moved")
        .expect("move an inode whose attribute fork is a leaf to another directory");
    fs.create_file(made, b"replacer", 0o100644).expect("create");
    fs.rename(made, b"replacer", made, b"hello-link")
        .expect("replace one of two names of an inode whose attribute fork is a leaf");
    fs.unlink_file(root, b"multi-too")
        .expect("unlink one of two names of an inode whose attribute fork is a leaf");
    // A node-form fork, the same three.
    fs.link(ino("/bulky"), made, b"bulky-link")
        .expect("link an inode whose attribute fork is a node");
    fs.rename(root, b"bulky", made, b"bulky-moved")
        .expect("move an inode whose attribute fork is a node to another directory");
    fs.unlink_file(root, b"bulky-too")
        .expect("unlink one of three names of an inode whose attribute fork is a node");
    // The leaf-form inode's name replaces one of the node-form inode's:
    // both forks are rebuilt in one record.
    fs.rename(made, b"hello-moved", made, b"bulky-link")
        .expect("a rename whose moved inode and replaced inode both have forks past short form");
    // Across the boundary. One 200-byte name's pointer fits short form and
    // two do not, so the second link moves the fork to a leaf; on a second
    // inode, unlinking the first name again leaves one pointer.
    let long: Vec<String> = ["a", "b", "c", "d"]
        .iter()
        .map(|c| format!("long-{c}-").repeat(25))
        .collect();
    let (crossing, _) = fs
        .create_file(made, long[0].as_bytes(), 0o100644)
        .expect("create");
    fs.link(crossing, made, long[1].as_bytes())
        .expect("a link whose parent pointer moves the attribute fork past short form");
    let (back, _) = fs
        .create_file(made, long[2].as_bytes(), 0o100644)
        .expect("create");
    fs.link(back, made, long[3].as_bytes())
        .expect("a link whose parent pointer moves the attribute fork past short form");
    fs.unlink_file(made, long[2].as_bytes())
        .expect("unlink one of two names of an inode whose fork crossed to a leaf");

    // THE LAST NAME (#495): the inode is freed with every block its
    // attribute fork holds -- a leaf, remote values and the B+tree mapping
    // them, a node -- by an unlink and by a rename that replaces it.
    fs.unlink_file(root, b"multi")
        .expect("unlink the last name of an inode whose attribute fork is a leaf");
    fs.unlink_file(root, b"treed").expect(
        "unlink the last name of an inode whose attribute fork maps remote values with a B+tree",
    );
    fs.create_file(made, b"over-noded", 0o100644)
        .expect("create");
    fs.rename(made, b"over-noded", root, b"noded")
        .expect("replace the last name of an inode whose attribute fork is a node");
    // Still refused (#501): a remote value beside the parent pointers.
    let before = image_digest(img);
    assert!(
        matches!(
            fs.link(ino("/remote"), made, b"remote-link"),
            Err(Error::UnsupportedFeature(_))
        ),
        "{tag}: a link to an inode whose attribute fork holds a remote value must be refused"
    );
    assert!(
        image_digest(img) == before,
        "{tag}: a refused link changed the image"
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

    let path = |name: &String| format!("/made/{name}");
    let crossing_links = [(long[0].as_str(), "/made"), (long[1].as_str(), "/made")];
    let back_links = [(long[3].as_str(), "/made")];
    let fs = Filesystem::mount(Arc::new(FileDevice::open(img).unwrap())).unwrap();
    for (path, links) in [
        ("/made".to_string(), &[("made", "/")][..]),
        ("/made/renamed.txt".into(), &[("renamed.txt", "/made")]),
        ("/made/sym".into(), &[("sym", "/made")]),
        ("/made/top-moved.txt".into(), &[("top-moved.txt", "/made")]),
        ("/many/entry_first".into(), &[("entry_first", "/many")]),
        ("/victim-too".into(), &[("victim-too", "/")]),
        ("/made/victim".into(), &[("victim", "/made")]),
        ("/made/dir-moved".into(), &[("dir-moved", "/made")]),
        (
            "/made/dir-moved/link".into(),
            &[("link", "/made/dir-moved")],
        ),
        (
            "/made/twice".into(),
            &[("twice", "/made"), ("twice-in-many", "/many")],
        ),
        // #488: the leaf-form inode, now named only by the rename that
        // replaced one of the node-form inode's names.
        ("/made/bulky-link".into(), &[("bulky-link", "/made")]),
        ("/made/bulky-moved".into(), &[("bulky-moved", "/made")]),
        ("/made/hello-link".into(), &[("hello-link", "/made")]),
        // #495: the file the rename put in place of a freed one.
        ("/noded".into(), &[("noded", "/")]),
        ("/remote".into(), &[("remote", "/"), ("remote-too", "/")]),
        (path(&long[0]), &crossing_links),
        (path(&long[1]), &crossing_links),
        (path(&long[3]), &back_links),
    ] {
        let file = fs
            .open(&path)
            .unwrap_or_else(|e| panic!("{tag} {path}: {e:?}"));
        let mut got = fs.parent_pointers(file.inode(), file.raw()).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let mut want: Vec<ParentPointer> = links
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
        want.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got, want,
            "{tag} {path}: its parent pointers after the writes"
        );
        // The independent reading of the same thing: xfs_db, walking the
        // fork in whatever form it is in.
        let mut want: Vec<(u64, u32, String)> = want
            .into_iter()
            .map(|p| (p.parent, p.generation, String::from_utf8(p.name).unwrap()))
            .collect();
        want.sort();
        assert_eq!(
            xfs_db_parents(img, &path),
            want,
            "{tag} {path}: xfs_db's listing of its parent pointers"
        );
    }
    // Every rebuild of a fork past short form kept the user attributes, and
    // the forks are still in the forms these cases are about.
    for (path, count, last, form) in [
        ("/made/bulky-link".to_string(), 30, "user.name_29", "leaf"),
        ("/made/bulky-moved".into(), 300, "user.name_299", "node"),
        (path(&long[0]), 0, "", "leaf"),
    ] {
        let file = fs.open(&path).unwrap();
        let xattrs = fs.list_xattrs(file.inode(), file.raw()).unwrap();
        assert_eq!(
            xattrs.len(),
            count,
            "{tag} {path}: its user attributes after the writes"
        );
        if count > 0 {
            let value = last.replace("user.name_", "value_");
            assert_eq!(
                fs.get_xattr(file.inode(), file.raw(), last.as_bytes())
                    .unwrap()
                    .as_deref(),
                Some(value.as_bytes()),
                "{tag} {path}: {last}"
            );
        }
        assert_eq!(
            xfs_db_attr_form(img, &path),
            form,
            "{tag} {path}: the form of its attribute fork after the writes"
        );
    }
    for gone in [
        "/many/entry_000".to_string(),
        "/many/entry_001".into(),
        "/many/entry_002".into(),
        "/made/empty".into(),
        "/dir".into(),
        "/made/dir-moved/hello.txt".into(),
        "/made/hello-moved".into(),
        "/made/replacer".into(),
        "/made/remote-link".into(),
        "/multi-too".into(),
        "/multi".into(),
        "/treed".into(),
        "/made/over-noded".into(),
        "/bulky".into(),
        "/bulky-too".into(),
        path(&long[2]),
    ] {
        assert!(
            fs.lookup_path(&gone).is_err(),
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
