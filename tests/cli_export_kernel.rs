//! `fs.xfs export`, judged by THE KERNEL.
//!
//! Linux writes the files on a copy of the kernel-made v5 CLI image in the
//! harness guest, and computes their SHA-256 there, from the kernel's own
//! mount: an empty file, a file of 3 MiB plus one byte (it crosses the
//! transfer's 1 MiB chunk boundaries and ends one byte into a fourth), and
//! a tree 34 directories deep with a file at the bottom. `fs.xfs export`
//! then copies them out on the host, and every file it delivers must have
//! the digest the kernel gave for the same path.
//!
//! THE DIGESTS ARE THE KERNEL'S, NOT OURS: this crate never reads the
//! files it is graded on before the guest has digested them.
//!
//! A damaged copy -- the extent list of the 3 MiB file made to point past
//! the end of the filesystem, by `xfs_db`, which recomputes the inode's
//! checksum -- must fail that file, with a nonzero exit and no destination
//! file, and leave the rest of the tree exported.
//!
//! EVERY GUEST STEP WORKS ON A COPY ON THE GUEST'S OWN DISK, for the
//! reason `cli_write_kernel.rs` gives: a loop mount of a file on the
//! shared folder mixes the guest's page cache with the host's view of it.

mod cli_support;
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, scratch};

const SUITE: &str = "cli_export_kernel";

/// How many directories deep the kernel's tree goes below `/exp`.
const DEPTH: usize = 34;

/// Every file below `dir`, by its path relative to it, with its SHA-256.
fn digests(dir: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, into: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(at).expect("read a directory of the export") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                walk(root, &path, into);
            } else {
                let rel = path.strip_prefix(root).unwrap().to_str().unwrap();
                into.insert(
                    format!("/{rel}"),
                    sha256_hex(&std::fs::read(&path).expect("read an exported file")),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(dir, dir, &mut found);
    found
}

fn kernel_digests(out: &str) -> BTreeMap<String, String> {
    out.lines()
        .filter_map(|l| l.strip_prefix("DIGEST "))
        .map(|l| {
            let (sum, path) = l.split_once(' ').expect("DIGEST SUM PATH");
            (path.to_string(), sum.to_string())
        })
        .collect()
}

#[test]
fn export_delivers_the_files_the_kernel_digested_and_fails_what_it_cannot_read() {
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-v5.img", std::process::id()),
    );
    let image = copy.path().to_str().unwrap().to_string();
    let image = image.as_str();
    let deep: String = (1..=DEPTH).map(|i| format!("d{i}/")).collect();

    let made = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        mkdir -p "$work/m/exp/{deep}"
        : > "$work/m/exp/empty"
        head -c 3145729 /dev/urandom > "$work/m/exp/b3m1.bin"
        head -c 100000 /dev/urandom > "$work/m/exp/{deep}leaf.bin"
        sync
        umount "$work/m" || echo UMOUNT_FAILED
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        ( cd "$work/m/exp" && find . -type f | sort | while read -r f; do
            echo "DIGEST $(sha256sum "$f" | cut -d' ' -f1) ${{f#.}}"
        done )
        umount "$work/m" || echo UMOUNT_FAILED
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(image),
        deep = deep,
    ));
    assert!(made.contains("DONE"), "the guest did not finish:\n{made}");
    assert!(
        !made.contains("UMOUNT_FAILED"),
        "the kernel did not finish with the volume:\n{made}"
    );
    let kernel = kernel_digests(&made);
    assert_eq!(
        kernel.len(),
        3,
        "the kernel digested {} files, not 3:\n{made}",
        kernel.len()
    );

    // One file at a time: the kernel's digest for each.
    let out_dir = scratch::dir(SUITE).join(format!("{}-out", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).unwrap();
    for (n, path) in ["/exp/empty", "/exp/b3m1.bin"].into_iter().enumerate() {
        let dest = out_dir.join(format!("one-{n}"));
        let out = ok(tool("fs.xfs").args([image, "export", path, dest.to_str().unwrap()]));
        let want = &kernel[path.trim_start_matches("/exp")];
        assert_eq!(
            sha256_hex(&std::fs::read(&dest).expect("the exported file")),
            *want,
            "export {path} is not the file the kernel digested"
        );
        assert_eq!(json_field(&stdout(&out), "sha256"), *want);
    }
    let size = std::fs::metadata(out_dir.join("one-1")).unwrap().len();
    assert_eq!(size, 3 * 1024 * 1024 + 1, "the exported file's length");

    // The tree: every file the kernel digested, at its path.
    let tree = out_dir.join("tree");
    let out = ok(tool("fs.xfs").args([
        image,
        "export",
        "--recursive",
        "/exp",
        tree.to_str().unwrap(),
    ]));
    let report = stdout(&out);
    assert_eq!(json_field(&report, "failed"), "0", "{report}");
    assert_eq!(
        digests(&tree),
        kernel,
        "the tree is not what the kernel digested"
    );
    assert!(
        kernel.keys().any(|p| p.matches('/').count() > DEPTH),
        "the tree is not {DEPTH} directories deep"
    );

    // A damaged copy: the 3 MiB file's extents point off the filesystem.
    let ino = json_field(
        &stdout(&ok(tool("fs.xfs").args([image, "stat", "/exp/b3m1.bin"]))),
        "inode",
    );
    let bad = scratch::Volume::copy_of(
        SUITE,
        copy.path(),
        &format!("{}-bad.img", std::process::id()),
    );
    let bad_image = bad.path().to_str().unwrap();
    let damaged = kernel_run(&format!(
        r#"
        work=$(mktemp -d)
        trap 'rm -rf "$work"' EXIT
        cp --sparse=always {bad} "$work/image"
        xfs_db -x -c "inode {ino}" -c "write u3.bmx[0].startblock 4000000000" "$work/image" && echo DAMAGED
        cp --sparse=always "$work/image" {bad}
        sync
        echo DONE
        "#,
        bad = guest_quote(bad_image),
        ino = ino,
    ));
    assert!(
        damaged.contains("DAMAGED"),
        "xfs_db did not damage the inode:\n{damaged}"
    );

    let lone = out_dir.join("lone");
    let out = tool("fs.xfs")
        .args([bad_image, "export", "/exp/b3m1.bin", lone.to_str().unwrap()])
        .output()
        .expect("run fs.xfs export");
    assert!(
        !out.status.success(),
        "an unreadable file was exported: {}",
        stdout(&out)
    );
    assert!(
        stdout(&out).is_empty(),
        "a failed export printed: {}",
        stdout(&out)
    );
    assert!(!lone.exists(), "a failed export left a destination file");
    assert!(
        PathBuf::from(&out_dir).read_dir().unwrap().all(|e| !e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .ends_with(".partial")),
        "a failed export left a staging file"
    );

    let damaged_tree = out_dir.join("damaged-tree");
    let out = tool("fs.xfs")
        .args([
            bad_image,
            "export",
            "--recursive",
            "/exp",
            damaged_tree.to_str().unwrap(),
        ])
        .output()
        .expect("run fs.xfs export --recursive");
    assert!(
        !out.status.success(),
        "a tree with an unreadable file exited 0: {}",
        stdout(&out)
    );
    assert!(
        !damaged_tree.join("b3m1.bin").exists(),
        "the unreadable file was published"
    );
    let rest = digests(&damaged_tree);
    for path in ["/empty", &format!("/{deep}leaf.bin")] {
        assert_eq!(
            rest.get(path),
            kernel.get(path),
            "{path}: the rest of the tree was not exported intact"
        );
    }
    let _ = std::fs::remove_dir_all(&out_dir);
}
