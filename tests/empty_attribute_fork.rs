//! Empty attribute forks using independently generated base-image inodes.
//! Linux v6.13's xfs_inode_hasattr in libxfs/xfs_attr.c returns false for
//! extent format with zero extents, even when an attribute fork exists.
//! Fork presence varies with the kernel that populated the image, so these
//! reader contracts set the in-memory fork fields explicitly. They do not
//! claim that the fixture's kernel produced an empty attribute fork.

mod common;

use fs_core::FileDevice;
use fs_xfs::extent::Extent;
use fs_xfs::inode::Format;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn mount() -> Filesystem {
    Filesystem::mount(Arc::new(
        FileDevice::open(common::fixture("xfsfeat-base.img")).unwrap(),
    ))
    .unwrap()
}

fn empty_fork(mut inode: fs_xfs::inode::Inode, raw: &[u8]) -> fs_xfs::inode::Inode {
    // Reserve the last 16 bytes for an extent-format attribute fork.
    inode.forkoff = u8::try_from((raw.len() - inode.data_fork_offset() - 16) / 8).unwrap();
    inode.aformat = Format::Extents;
    inode.anextents = 0;
    inode
}

#[test]
fn cached_directories_with_empty_extent_attribute_forks_have_no_attributes() {
    let fs = mount();
    // Parents of the four creation-contract failures, including a later AG.
    let spread = fs.lookup_path("/spread").unwrap();
    let (spread, raw) = fs.read_inode_raw(spread.ino).unwrap();
    let later = fs
        .read_dir(&spread, &raw)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.name != b"." && entry.name != b"..")
        .find(|entry| fs.superblock().split_ino(entry.ino).0 > 0)
        .expect("the base fixture has a directory in a later AG")
        .ino;
    for ino in [
        fs.lookup_path("/sf").unwrap().ino,
        fs.lookup_path("/full").unwrap().ino,
        later,
    ] {
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        assert!(inode.is_dir());
        let inode = empty_fork(inode, &raw);
        assert_ne!(inode.forkoff, 0);
        assert_eq!(inode.aformat, Format::Extents);
        assert_eq!(inode.anextents, 0);
        assert!(fs.list_xattrs(&inode, &raw).unwrap().is_empty());
        assert_eq!(
            fs.get_xattr(&inode, &raw, b"system.posix_acl_default")
                .unwrap(),
            None
        );
    }
}

#[test]
fn a_nonempty_attribute_fork_still_requires_block_zero() {
    let fs = mount();
    let ino = fs.lookup_path("/sf").unwrap().ino;
    let (mut inode, mut raw) = fs.read_inode_raw(ino).unwrap();
    inode = empty_fork(inode, &raw);
    inode.anextents = 1;
    let (start, _) = inode.attr_fork_range(raw.len()).unwrap();
    raw[start..start + 16].copy_from_slice(
        &Extent {
            startoff: 1,
            startblock: 1,
            blockcount: 1,
            unwritten: false,
        }
        .to_bytes()
        .unwrap(),
    );
    let error = fs.list_xattrs(&inode, &raw).unwrap_err();
    assert!(error.to_string().contains("block 0 is a hole"), "{error}");
}

#[test]
fn an_empty_attribute_fork_still_requires_a_complete_inode_record() {
    let fs = mount();
    let ino = fs.lookup_path("/sf").unwrap().ino;
    let (inode, raw) = fs.read_inode_raw(ino).unwrap();
    let inode = empty_fork(inode, &raw);
    assert_eq!(inode.anextents, 0);
    let error = fs.list_xattrs(&inode, &raw[..raw.len() - 1]).unwrap_err();
    assert!(error.to_string().contains("fork past the inode record"));
}
