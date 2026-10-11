//! POSIX ACLs are set, kept in step with the mode, removed and inherited
//! (#390).
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on an in-memory device. What the kernel's own ACL
//! code makes of the same requests is `tests/acl_write_oracle.rs`, and
//! inheritance on create against the kernel's is
//! `tests/create_default_acl_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::acl::{AclEntry, AclKind};
use fs_xfs::format::acl::tag;
use fs_xfs::write::AttrChange;
use fs_xfs::{Error, Filesystem};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: usize = 512;

struct Sparse(Mutex<BTreeMap<u64, Box<[u8; SECTOR]>>>);

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            match sectors.get(&sector) {
                Some(s) => buf[done..done + n].copy_from_slice(&s[within..within + n]),
                None => buf[done..done + n].fill(0),
            }
            done += n;
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut sectors = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let (sector, within) = (at / SECTOR as u64, (at % SECTOR as u64) as usize);
            let n = (SECTOR - within).min(buf.len() - done);
            sectors
                .entry(sector)
                .or_insert_with(|| Box::new([0; SECTOR]))[within..within + n]
                .copy_from_slice(&buf[done..done + n]);
            done += n;
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

fn mounted() -> Filesystem {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw")
}

fn e(tag: u32, id: u32, perm: u16) -> AclEntry {
    AclEntry { tag, id, perm }
}

fn perm(fs: &Filesystem, ino: u64) -> u16 {
    fs.read_inode(ino).expect("inode").mode & 0o7777
}

fn named() -> Vec<AclEntry> {
    vec![
        e(tag::USER_OBJ, 0, 6),
        e(tag::USER, 1000, 4),
        e(tag::GROUP_OBJ, 0, 4),
        e(tag::MASK, 0, 6),
        e(tag::OTHER, 0, 0),
    ]
}

#[test]
fn an_access_acl_sets_the_mode_it_implies_and_reads_back() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (f, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    fs.set_acl(f, AclKind::Access, &named()).expect("set");
    assert_eq!(fs.acl(f, AclKind::Access).expect("read"), Some(named()));
    assert_eq!(perm(&fs, f), 0o660, "the mask is the group bits");
}

#[test]
fn an_acl_the_mode_already_says_is_stored_as_the_mode() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (f, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    fs.set_acl(
        f,
        AclKind::Access,
        &[
            e(tag::USER_OBJ, 0, 7),
            e(tag::GROUP_OBJ, 0, 5),
            e(tag::OTHER, 0, 1),
        ],
    )
    .expect("set");
    assert_eq!(fs.acl(f, AclKind::Access).expect("read"), None);
    assert_eq!(perm(&fs, f), 0o751);
}

#[test]
fn chmod_moves_an_acls_base_entries_and_set_attributes_will_not() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (f, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    fs.set_acl(f, AclKind::Access, &named()).expect("set");
    fs.chmod(f, 0o750).expect("chmod");
    assert_eq!(perm(&fs, f), 0o750);
    let acl = fs.acl(f, AclKind::Access).expect("read").expect("an acl");
    let p = |t: u32| acl.iter().find(|x| x.tag == t).expect("entry").perm;
    assert_eq!((p(tag::USER_OBJ), p(tag::MASK), p(tag::OTHER)), (7, 5, 0));
    assert_eq!(
        p(tag::GROUP_OBJ),
        4,
        "the owning group is left alone under a mask"
    );
    let inode = fs.read_inode(f).expect("inode");
    let refused = fs.set_attributes(
        &inode,
        &AttrChange {
            permissions: Some(0o700),
            ..AttrChange::default()
        },
    );
    assert!(
        matches!(refused, Err(Error::UnsupportedFeature(_))),
        "{refused:?}"
    );
}

#[test]
fn removing_an_acl_keeps_the_mode_and_a_default_needs_a_directory() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (f, _) = fs.create_file(root, b"f", 0o100644).expect("create");
    fs.set_acl(f, AclKind::Access, &named()).expect("set");
    fs.remove_acl(f, AclKind::Access).expect("remove");
    assert_eq!(fs.acl(f, AclKind::Access).expect("read"), None);
    assert_eq!(perm(&fs, f), 0o660);
    assert!(matches!(
        fs.set_acl(f, AclKind::Default, &named()),
        Err(Error::NotADirectory)
    ));
}

#[test]
fn a_new_inode_inherits_its_directorys_default() {
    let fs = mounted();
    let root = fs.superblock().rootino;
    let (d, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let default = vec![
        e(tag::USER_OBJ, 0, 7),
        e(tag::USER, 1000, 7),
        e(tag::GROUP_OBJ, 0, 5),
        e(tag::MASK, 0, 7),
        e(tag::OTHER, 0, 5),
    ];
    fs.set_acl(d, AclKind::Default, &default).expect("default");
    let (f, _) = fs
        .create_file(d, b"f", 0o100640)
        .expect("create under a default");
    assert_eq!(
        perm(&fs, f),
        0o640,
        "the mode asked for narrows the default"
    );
    let access = fs
        .acl(f, AclKind::Access)
        .expect("read")
        .expect("inherited");
    assert!(access.iter().any(|x| x.tag == tag::USER && x.id == 1000));
    let (sub, _) = fs
        .create_directory(d, b"sub", 0o040755)
        .expect("mkdir under a default");
    assert_eq!(fs.acl(sub, AclKind::Default).expect("read"), Some(default));
}
