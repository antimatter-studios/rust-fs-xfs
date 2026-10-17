//! Recursive metadata against find on a Linux kernel mount, never our fixtures alone.
mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, scratch};
use fs_xfs::{inode::Format, Filesystem};
use std::collections::BTreeMap;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

fn agrees(name: &str, options: &str) {
    let image = scratch::Volume::empty("cli-recursive-kernel", name, 320 << 20);
    let report = kernel_run(&format!(
        r#"
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -i size=512 {options} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        ( cd "$work/m"
          mkdir -p a/deep empty z
          printf payload > a/deep/file
          printf space > 'two words'
          ln a/deep/file hard
          ln -s a dirlink
          ln -s ../.. a/deep/up
          ln -s absent dangling
          printf raw > a/deep/$'raw-\xfe'
          printf raw > a/deep/$'raw-\xff'
        )
        sync
        umount "$work/m"
        mount -o loop,ro,nouuid,norecovery "$work/image" "$work/m"
        ( cd "$work/m"
          find . -mindepth 1 -printf '%P\0%y\0%s\0%i\0' | od -An -v -tx1 | sed 's/^/FIND /'
        )
        umount "$work/m"
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(image.path().to_str().unwrap()),
    ));
    let bytes: Vec<u8> = report
        .lines()
        .filter_map(|l| l.strip_prefix("FIND "))
        .flat_map(str::split_whitespace)
        .map(|b| u8::from_str_radix(b, 16).unwrap())
        .collect();
    let fields: Vec<_> = bytes.split(|b| *b == 0).collect();
    assert_eq!(fields.last(), Some(&b"".as_slice()));
    assert_eq!((fields.len() - 1) % 4, 0);
    let number = |b: &[u8]| std::str::from_utf8(b).unwrap().parse::<u64>().unwrap();
    let mut expected = BTreeMap::new();
    for row in fields[..fields.len() - 1].chunks_exact(4) {
        let kind = match row[1] {
            b"d" => "dir",
            b"f" => "file",
            b"l" => "symlink",
            _ => panic!("unexpected find type"),
        };
        expected.insert(
            [b"/", row[0]].concat(),
            (kind.to_string(), number(row[2]), number(row[3])),
        );
    }
    assert_eq!(expected.len(), 12, "find must cover the entire tree");
    let img = image.path().to_str().unwrap();
    let before = sha256_hex(&std::fs::read(img).unwrap());
    let output = stdout(&ok(tool("fs.xfs").args([img, "ls", "-R", "/"])));
    assert_eq!(
        output,
        stdout(&ok(tool("fs.xfs").args([img, "ls", "-R", "/"])))
    );
    let mut actual = BTreeMap::new();
    let mut paths = Vec::new();
    for row in output.split("\n  {").skip(1) {
        let path = if row.contains("\"path_hex\"") {
            let hex = json_field(row, "path_hex");
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
                .collect::<Vec<_>>()
        } else {
            json_field(row, "path").into_bytes()
        };
        assert_eq!(
            json_field(row, "depth").parse::<usize>().unwrap(),
            path.iter().filter(|b| **b == b'/').count()
        );
        assert!(!row.contains("\"revisited\""));
        paths.push(path.clone());
        assert!(actual
            .insert(
                path,
                (
                    json_field(row, "type"),
                    json_field(row, "size").parse::<u64>().unwrap(),
                    json_field(row, "inode").parse::<u64>().unwrap()
                )
            )
            .is_none());
    }
    assert_eq!(actual, expected);
    let mut sorted = paths.clone();
    sorted.sort_by(|a, b| a.split(|v| *v == b'/').cmp(b.split(|v| *v == b'/')));
    assert_eq!(
        paths, sorted,
        "byte-sorted depth-first order, including raw names"
    );
    let empty = stdout(&ok(tool("fs.xfs").args([
        img,
        "ls",
        "-R",
        "--limit",
        "0",
        "--max-depth",
        "0",
        "/empty",
    ])));
    assert_eq!(empty.trim(), "[]", "zero bounds accept an empty directory");
    assert_eq!(sha256_hex(&std::fs::read(img).unwrap()), before);
    if name == "v4.img" {
        faults(&image);
    }
}

// Faults are applied only to a private v4 copy of the independently made image;
// no checksum is bypassed. The short-form offsets are the documented dir::offsets.
fn faults(source: &scratch::Volume) {
    let fs =
        Filesystem::mount(Arc::new(fs_core::FileDevice::open(source.path()).unwrap())).unwrap();
    let deep = fs.open("/a/deep").unwrap();
    for fault in ["duplicate", "cycle", "unreadable"] {
        let image = scratch::Volume::copy_of(
            "cli-recursive-faults",
            source.path(),
            &format!("{fault}.img"),
        );
        let at = std::fs::OpenOptions::new()
            .write(true)
            .open(image.path())
            .unwrap();
        let (parent, name, replacement) = match fault {
            "duplicate" => (
                fs.root().unwrap(),
                b"z".as_slice(),
                fs.open("/a").unwrap().inode().ino,
            ),
            "cycle" => (
                fs.open("/a").unwrap(),
                b"deep".as_slice(),
                fs.root().unwrap().inode().ino,
            ),
            _ => (fs.open("/a/deep").unwrap(), b"".as_slice(), 0),
        };
        assert_eq!(parent.inode().format, Format::Local);
        let start = parent.inode().data_fork_range(parent.raw().len()).0;
        let base = fs.inode_offset(parent.inode().ino).unwrap() + start as u64;
        if fault == "unreadable" {
            at.write_all_at(&[255], base).unwrap();
        } else {
            let raw = &parent.raw()[start..];
            assert_eq!(raw[1], 0, "small fixture uses narrow inode numbers");
            let width = 4;
            let mut cur = fs_xfs::dir::offsets::sf_hdr::PARENT + width;
            let mut found = false;
            for _ in 0..raw[0] {
                let n = usize::from(raw[cur]);
                let end = cur + fs_xfs::dir::offsets::sf_entry::NAME + n;
                let ftype = usize::from(fs.superblock().has_ftype());
                if &raw[cur + 3..end] == name {
                    at.write_all_at(
                        &(replacement as u32).to_be_bytes(),
                        base + (end + ftype) as u64,
                    )
                    .unwrap();
                    found = true;
                    break;
                }
                cur = end + ftype + width;
            }
            assert!(found, "fault must replace an existing name");
        }
        let img = image.path().to_str().unwrap();
        let before = sha256_hex(&std::fs::read(img).unwrap());
        let out = tool("fs.xfs")
            .args([img, "ls", "-R", "--limit", "30", "--max-depth", "10", "/"])
            .output()
            .unwrap();
        if fault == "unreadable" {
            assert_eq!(out.status.code(), Some(1), "{out:?}");
            assert_eq!(json_field(&stderr(&out), "code"), "1");
            assert!(stderr(&out).contains("/a/deep"));
            assert_eq!(stdout(&out).matches("\"path\":").count(), 2);
        } else {
            assert!(out.status.success(), "{fault}: {}", stderr(&out));
            assert!(stdout(&out).contains("\"revisited\": true"));
            assert!(!stdout(&out).contains(if fault == "cycle" {
                "/a/deep/file"
            } else {
                "/z/deep"
            }));
        }
        assert_eq!(sha256_hex(&std::fs::read(img).unwrap()), before);
    }
    assert!(deep.is_dir());
}

#[test]
fn recursive_listing_matches_kernel_find_on_v5() {
    agrees("v5.img", "-m crc=1");
}

#[test]
fn recursive_listing_matches_kernel_find_and_terminates_on_v4_faults() {
    agrees("v4.img", "-m crc=0");
}
