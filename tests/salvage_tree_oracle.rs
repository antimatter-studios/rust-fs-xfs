//! Directory listings and the rebuilt tree against `xfs_db` and
//! `xfs_repair -n`, in the harness guest.
//!
//! `xfs_db`'s `ls` lists a directory with xfsprogs' own code: every name and
//! the inode it points at. The salvage listing of the same directory has to
//! hold the same pairs. A directory inode `xfs_db -x` destroys has to leave
//! its children as orphans the tree still holds, and `xfs_repair -n` has to
//! call that copy damaged.

mod common;

use common::{fixture, oracle, scratch};
use fs_core::FileDevice;
use fs_xfs::salvage::{self, Extent};
use fs_xfs::Filesystem;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;

const SUITE: &str = "salvage_tree_oracle";

fn open(path: &Path) -> Arc<FileDevice> {
    Arc::new(FileDevice::open(path).unwrap_or_else(|e| panic!("opening {}: {e}", path.display())))
}

/// `(name, inode)` pairs from `xfs_db`'s `ls`, whose lines read
/// `8          128 directory      0x0000002e   1 . (good)`: the inode number
/// is the second word and the name the one before the hash state.
fn listed(text: &str) -> BTreeSet<(String, u64)> {
    text.lines()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            if words.len() < 6 {
                return None;
            }
            let ino = words[1].parse().ok()?;
            let name = words[words.len() - 2].to_string();
            (name != "." && name != "..").then_some((name, ino))
        })
        .collect()
}

/// The salvage listings of three directories, one per shape, hold exactly
/// the names `xfs_db` lists.
#[test]
fn listings_agree_with_xfs_db_ls() {
    let image = fixture("xfsdata-default.img");
    let device = open(&image);
    let fs = Filesystem::mount(device.clone()).unwrap();
    let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
    for path in ["/", "/sub", "/manyfiles"] {
        let ino = fs.lookup_path(path).unwrap().ino;
        let out = oracle("xfs_db")
            .args(["-r", "-c", &format!("inode {ino}"), "-c", "ls"])
            .arg(image.to_str().unwrap())
            .output();
        assert!(
            out.ok(),
            "{path}: xfs_db failed:\n{}{}",
            out.stdout,
            out.stderr
        );
        let theirs = listed(&out.stdout);
        assert!(
            !theirs.is_empty(),
            "{path}: xfs_db listed nothing:\n{}",
            out.stdout
        );
        let candidate = salvage::read_inode(device.as_ref(), &sb, ino).unwrap();
        let listing = salvage::directory_entries(device.as_ref(), &sb, &candidate);
        assert!(listing.damage.is_empty(), "{path}: {:#?}", listing.damage);
        let ours: BTreeSet<(String, u64)> = listing
            .entries
            .iter()
            .map(|e| (String::from_utf8_lossy(&e.name).into_owned(), e.ino))
            .collect();
        assert_eq!(ours, theirs, "{path}");
    }
}

/// A directory inode `xfs_db` destroys leaves its file an orphan the tree
/// still holds, and `xfs_repair -n` calls the copy damaged.
#[test]
fn a_directory_xfs_db_destroys_leaves_orphans() {
    let image = fixture("xfsdata-default.img");
    let (dir_ino, file_ino) = {
        let fs = Filesystem::mount(open(&image)).unwrap();
        (
            fs.lookup_path("/sub/nested").unwrap().ino,
            fs.lookup_path("/sub/nested/file.txt").unwrap().ino,
        )
    };
    let copy = scratch::Volume::copy_of(
        SUITE,
        &image,
        &format!("{}-dir-magic.img", std::process::id()),
    );
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            &format!("inode {dir_ino}"),
            "-c",
            "write -d core.magic 0",
        ])
        .arg(copy.path().to_str().unwrap())
        .output();
    assert!(
        edit.ok(),
        "xfs_db could not break the inode:\n{}{}",
        edit.stdout,
        edit.stderr
    );

    let device = open(copy.path());
    let sb = salvage::superblocks(device.as_ref()).geometry.unwrap();
    let length = std::fs::metadata(copy.path()).unwrap().len();
    let mut candidates = Vec::new();
    salvage::find_inodes(device.as_ref(), &sb, Extent::new(0, length), &mut |c| {
        if c.state == salvage::InodeState::Live {
            candidates.push(c);
        }
        true
    });
    assert!(candidates.iter().all(|c| c.ino != dir_ino));
    let tree = salvage::rebuild_tree(device.as_ref(), &sb, &candidates);
    assert!(
        tree.orphans().any(|n| n.ino == file_ino),
        "the file under the destroyed directory is not an orphan"
    );

    let report = oracle("xfs_repair")
        .args(["-n", copy.path().to_str().unwrap()])
        .output()
        .repair_report();
    assert!(
        !report.contains(common::repair::IGNORED_THE_LOG),
        "xfs_repair ignored the log (\"valuable metadata changes in a log\"), so its report says nothing about this copy:\n{report}"
    );
    assert!(
        report.contains("REPAIR_RC=") && !report.contains("REPAIR_RC=0"),
        "xfs_repair -n found nothing wrong:\n{report}"
    );
    assert!(
        report.contains(&dir_ino.to_string()),
        "xfs_repair -n does not name inode {dir_ino}:\n{report}"
    );
}
