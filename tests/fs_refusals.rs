//! What the driver refuses, and why.
//!
//! The happy paths are covered by `endtoend_oracle.rs`. This file covers
//! the other half of correctness: the cases where the right answer is an
//! error rather than a best effort.
//!
//! That distinction matters more here than in most code. A filesystem
//! driver that returns *something* for a case it does not understand
//! hands a user silently wrong data, and they have no way to tell. Every
//! refusal below is a place where returning plausible bytes would be
//! worse than failing.
//!
//! Fixtures are gitignored; these skip cleanly without them.

use fs_core::FileDevice;
use fs_xfs::{Error, Filesystem};
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn any_fixture() -> Option<PathBuf> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let p = share.join("xfsdata-default.img");
    p.exists().then_some(p)
}

fn mount() -> Option<Filesystem> {
    let img = any_fixture()?;
    let dev = FileDevice::open(&img).expect("open image");
    Some(Filesystem::mount(Arc::new(dev)).expect("mount"))
}

#[test]
fn listing_a_regular_file_is_refused() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let found = fs.lookup_path("/small.txt").expect("small.txt exists");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
    assert!(
        matches!(fs.read_dir(&inode, &raw), Err(Error::NotADirectory)),
        "reading a regular file as a directory must be refused"
    );
}

#[test]
fn reading_a_directory_as_a_file_is_refused() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let found = fs.lookup_path("/sub").expect("sub exists");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
    assert!(
        matches!(fs.read_file(&inode, &raw), Err(Error::NotAFile)),
        "reading a directory's bytes as file contents must be refused"
    );
}

#[test]
fn readlink_on_a_regular_file_is_refused() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let found = fs.lookup_path("/small.txt").expect("small.txt exists");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
    assert!(matches!(fs.read_link(&inode, &raw), Err(Error::NotAFile)));
}

#[test]
fn a_missing_name_is_not_found() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    assert!(matches!(
        fs.lookup_path("/definitely-not-here.txt"),
        Err(Error::NotFound)
    ));
    assert!(matches!(
        fs.lookup_path("/sub/nested/also-absent"),
        Err(Error::NotFound)
    ));
}

#[test]
fn descending_through_a_file_is_refused() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    // `small.txt` is a file, so it cannot have children. Returning
    // NotFound here would be misleading — the path is malformed, not
    // merely absent.
    assert!(matches!(
        fs.lookup_path("/small.txt/child"),
        Err(Error::NotADirectory)
    ));
}

/// `..` is refused rather than silently resolved, so that a caller
/// cannot accidentally escape the subtree it thinks it is walking.
#[test]
fn parent_references_are_refused_rather_than_resolved() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    assert!(matches!(
        fs.lookup_path("/sub/../small.txt"),
        Err(Error::UnsupportedFeature(_))
    ));
}

/// Redundant separators and `.` components are ordinary, not errors.
#[test]
fn redundant_path_components_are_tolerated() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let direct = fs.lookup_path("/sub/nested/file.txt").expect("direct");
    for messy in [
        "//sub//nested//file.txt",
        "/./sub/./nested/./file.txt",
        "sub/nested/file.txt",
    ] {
        let got = fs
            .lookup_path(messy)
            .unwrap_or_else(|e| panic!("`{messy}` should resolve: {e}"));
        assert_eq!(
            got.ino, direct.ino,
            "`{messy}` resolved to a different inode"
        );
    }
}

/// The root path in its various spellings is the root directory.
#[test]
fn root_resolves_in_every_spelling() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let expected = fs.superblock().rootino;
    for spelling in ["/", "", ".", "/./"] {
        let got = fs.lookup_path(spelling).expect("root resolves");
        assert_eq!(
            got.ino, expected,
            "`{spelling}` did not resolve to the root"
        );
    }
}

#[test]
fn an_inode_number_outside_the_filesystem_is_refused() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    // An inode number whose allocation group index exceeds agcount.
    let sb = fs.superblock();
    let bogus = u64::from(sb.agcount + 5) << (sb.inopblog + sb.agblklog);
    assert!(
        fs.read_inode(bogus).is_err(),
        "an inode number naming a nonexistent allocation group must be refused"
    );
}

#[test]
fn reading_past_end_of_file_returns_nothing() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let found = fs.lookup_path("/small.txt").expect("small.txt");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
    let mut buf = [0u8; 64];
    let n = fs
        .read_at(&inode, &raw, inode.size + 100, &mut buf)
        .expect("reading past the end is not an error");
    assert_eq!(n, 0, "a read starting past EOF must return no bytes");
}

/// A read that starts inside the file but asks for more than remains is
/// short, not an error.
#[test]
fn reads_are_clamped_to_the_end_of_the_file() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let found = fs.lookup_path("/small.txt").expect("small.txt");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
    let mut buf = vec![0u8; inode.size as usize * 4];
    let n = fs.read_at(&inode, &raw, 1, &mut buf).expect("read");
    assert_eq!(
        n as u64,
        inode.size - 1,
        "a read from offset 1 must stop at the end of the file"
    );
}

/// Every allocation group's headers must be readable and self-identify.
/// This is the accessor path `endtoend_oracle` never touches.
#[test]
fn every_allocation_group_header_is_readable() {
    let Some(fs) = mount() else {
        eprintln!("no fixture — skipping");
        return;
    };
    let agcount = fs.superblock().agcount;
    for ag in 0..agcount {
        let agf = fs.read_agf(ag).unwrap_or_else(|e| panic!("AGF {ag}: {e}"));
        let agi = fs.read_agi(ag).unwrap_or_else(|e| panic!("AGI {ag}: {e}"));
        assert_eq!(agf.seqno, ag);
        assert_eq!(agi.seqno, ag);
    }
    // Past the end there is no allocation group to read.
    assert!(
        fs.read_agf(agcount).is_err(),
        "reading an allocation group beyond agcount must fail"
    );
}

/// Mounting something that is not XFS must be refused by magic, not
/// misparsed into nonsense geometry.
#[test]
fn mounting_a_non_xfs_device_is_refused() {
    let tmp = std::env::temp_dir().join(format!("fs-xfs-notxfs-{}.img", std::process::id()));
    std::fs::write(&tmp, vec![0xAAu8; 65536]).expect("write scratch image");
    let dev = FileDevice::open(&tmp).expect("open");
    match Filesystem::mount(Arc::new(dev)) {
        Err(Error::NotXfs { .. }) => {}
        Err(other) => panic!("expected a magic rejection, got {other}"),
        Ok(_) => panic!("a device full of 0xAA must not mount as XFS"),
    }
    std::fs::remove_file(&tmp).ok();
}
