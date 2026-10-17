//! A group whose AGF or AGI cannot be read is left unchecked, not
//! reported lost (#489).
//!
//! The checker claims a group's headers, and the log where it lives
//! there, before it reads the group's AGF and AGI. When either cannot be
//! read, nothing else in the group is claimed: its free space, its btree
//! blocks, its inode chunks and its files' blocks were never looked at.
//! Those blocks are unchecked, as `docs/fsck-output.md` says of a partial
//! scan, not lost; and the superblock's counters cannot be compared with
//! totals the group never added to.
//!
//! A btree that cannot be walked is the same thing on a smaller scale
//! (#496): its own blocks, and the free extents or inode chunks it would
//! have listed, are never claimed, and the group's counters cannot be
//! compared with records that were never read.
//!
//! An inode in a group whose inode btree was not read is not known to be
//! free or allocated (#497). An entry naming it is not an entry into
//! nothing; read on its own and found in use, it is walked like any
//! other, and the blocks it owns elsewhere are claimed. A quota inode
//! there, which no directory reaches, is read on its own the same way
//! before the quota check runs (#507).
//!
//! Each case damages one structure on a volume this crate's `mkfs` made
//! in memory, so it needs no tool and no fixture. What `xfs_repair -n`
//! makes of a damaged AGF and a damaged free-space btree is
//! `a_group_that_cannot_be_read_is_a_partial_scan` and
//! `a_free_space_btree_that_cannot_be_walked_is_a_partial_scan` in
//! `tests/cli_fsck_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::check::{Code, Report, Scan};
use fs_xfs::Filesystem;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 400 * 1024 * 1024;
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

/// The group whose header is damaged: the last of four.
const GROUP: u32 = 3;

fn volume() -> Arc<Sparse> {
    let dev = Arc::new(Sparse(Mutex::new(BTreeMap::new())));
    let options = fs_xfs::mkfs::Options {
        agcount: Some(4),
        ..fs_xfs::mkfs::Options::default()
    };
    fs_xfs::mkfs::format(dev.as_ref(), &options).expect("mkfs");
    dev
}

/// Mount `dev` with no block cache, so a sector damaged after the mount
/// is what the check reads.
fn mount(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount_with_cache(dev.clone() as Arc<dyn BlockRead>, 0).expect("mount")
}

/// Zero the magic of the header in sector `sector` of group `GROUP`:
/// the AGF is sector 1, the AGI sector 2.
fn damage_magic(dev: &Arc<Sparse>, sector: u64) {
    let sb = mount(dev).superblock().clone();
    let at = u64::from(GROUP) * u64::from(sb.agblocks) * u64::from(sb.blocksize)
        + sector * u64::from(sb.sectsize);
    let magic = fs_xfs::ag::offsets::common::MAGIC as u64;
    dev.write_at(at + magic, &[0; 4]).unwrap();
}

/// What the check found, one finding to a line.
fn listed(report: &Report) -> String {
    report
        .findings
        .iter()
        .map(|f| format!("{} ag {:?}: {}", f.code.as_str(), f.location.ag, f.what))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The report says the group could not be read, says the scan is
/// partial, and says nothing the unread group would have had to answer
/// for: no lost blocks in it, and no superblock counter against totals
/// it never added to.
fn assert_unchecked_not_lost(report: &Report, unreadable: Code, header: &str) {
    let all = listed(report);
    assert_eq!(
        report.scan,
        Scan::Partial,
        "a group whose {header} cannot be read is not a complete scan:\n{all}"
    );
    assert!(
        !report.is_clean(),
        "a group whose {header} cannot be read is called clean:\n{all}"
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.code == unreadable && f.location.ag == Some(GROUP)),
        "no {} for group {GROUP}:\n{all}",
        unreadable.as_str()
    );
    let lost: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.code == Code::Lost && f.location.ag == Some(GROUP))
        .collect();
    assert!(
        lost.is_empty(),
        "the blocks of group {GROUP}, whose {header} cannot be read, are reported lost:\n{all}"
    );
    let counters: Vec<_> = report
        .findings
        .iter()
        .filter(|f| {
            matches!(
                f.code,
                Code::CounterSbIcount | Code::CounterSbIfree | Code::CounterSbFdblocks
            )
        })
        .collect();
    assert!(
        counters.is_empty(),
        "the superblock's counters are compared with totals group {GROUP}, whose {header} \
         cannot be read, never added to:\n{all}"
    );
}

#[test]
fn a_fresh_volume_checks_clean() {
    let report = fs_xfs::check::check(&mount(&volume()));
    assert!(
        report.is_clean(),
        "the fresh volume has findings:\n{}",
        listed(&report)
    );
}

#[test]
fn a_group_whose_agf_cannot_be_read_is_unchecked_not_lost() {
    let dev = volume();
    damage_magic(&dev, 1);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unchecked_not_lost(&report, Code::AgfUnreadable, "AGF");
}

/// A damaged AGI on a volume whose log is clean is refused at mount:
/// the mount reads every group's AGI for its unlinked list. The checker
/// reaches one it cannot read on a volume whose log was replayed, which
/// skips that read, or when the read fails after the mount; this case is
/// the second, which needs no log to be written first.
#[test]
fn a_group_whose_agi_cannot_be_read_is_unchecked_not_lost() {
    let dev = volume();
    let fs = mount(&dev);
    damage_magic(&dev, 2);
    let report = fs_xfs::check::check(&fs);
    assert_unchecked_not_lost(&report, Code::AgiUnreadable, "AGI");
}

/// Zero the magic of block `agbno` of group `ag`, so the btree it
/// belongs to cannot be walked.
fn damage_block(dev: &Arc<Sparse>, ag: u32, agbno: u32) {
    let sb = mount(dev).superblock().clone();
    let at = (u64::from(ag) * u64::from(sb.agblocks) + u64::from(agbno)) * u64::from(sb.blocksize);
    dev.write_at(at, &[0; 4]).unwrap();
}

/// The scan is partial, and `btree.unreadable` names group `ag`.
fn assert_unwalked(report: &Report, ag: u32, tree: &str) {
    let all = listed(report);
    assert_eq!(
        report.scan,
        Scan::Partial,
        "a {tree} btree that cannot be walked is not a complete scan:\n{all}"
    );
    assert!(
        report
            .findings
            .iter()
            .any(|f| f.code == Code::BtreeUnreadable && f.location.ag == Some(ag)),
        "no btree.unreadable for group {ag}'s {tree} btree:\n{all}"
    );
}

/// None of `in_group` is reported for group `ag`, and no superblock
/// counter at all: each would be a claim about records nobody read.
fn assert_none_derived(report: &Report, ag: u32, in_group: &[Code], tree: &str) {
    let sb_counters = [
        Code::CounterSbIcount,
        Code::CounterSbIfree,
        Code::CounterSbFdblocks,
    ];
    let derived: Vec<&str> = report
        .findings
        .iter()
        .filter(|f| {
            (in_group.contains(&f.code) && f.location.ag == Some(ag))
                || sb_counters.contains(&f.code)
        })
        .map(|f| f.code.as_str())
        .collect();
    assert!(
        derived.is_empty(),
        "group {ag}'s {tree} btree cannot be walked, and the check still reports {derived:?} \
         from what it did not read:\n{}",
        listed(report)
    );
}

/// What a free-space btree that cannot be walked would otherwise be
/// blamed for: its group's free extents and its own blocks as lost, the
/// two trees as disagreeing, and the AGF's counts as wrong.
const FREE_SPACE_DERIVED: [Code; 5] = [
    Code::Lost,
    Code::FreespDisagree,
    Code::CounterAgfFreeblks,
    Code::CounterAgfLongest,
    Code::CounterAgfBtreeblks,
];

#[test]
fn a_free_space_by_block_btree_that_cannot_be_walked_is_unchecked_not_lost() {
    use fs_xfs::ag::agf_btree::BNO;
    let dev = volume();
    let root = mount(&dev).read_agf(GROUP).expect("the AGF").roots[BNO];
    damage_block(&dev, GROUP, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, GROUP, "free-space-by-block");
    assert_none_derived(&report, GROUP, &FREE_SPACE_DERIVED, "free-space-by-block");
}

#[test]
fn a_free_space_by_count_btree_that_cannot_be_walked_is_unchecked_not_lost() {
    use fs_xfs::ag::agf_btree::CNT;
    let dev = volume();
    let root = mount(&dev).read_agf(GROUP).expect("the AGF").roots[CNT];
    damage_block(&dev, GROUP, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, GROUP, "free-space-by-count");
    assert_none_derived(&report, GROUP, &FREE_SPACE_DERIVED, "free-space-by-count");
}

/// Group 0, because it holds the root directory's chunk, the one chunk a
/// fresh volume has: an inode btree that cannot be walked there leaves
/// that chunk unlisted, unclaimed and uncounted.
#[test]
fn an_inode_btree_that_cannot_be_walked_is_unchecked_not_lost() {
    let dev = volume();
    let root = mount(&dev).read_agi(0).expect("the AGI").root;
    damage_block(&dev, 0, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, 0, "inode");
    assert_none_derived(
        &report,
        0,
        &[Code::Lost, Code::CounterAgiInodes, Code::FinobtMismatch],
        "inode",
    );
}

/// A volume whose root holds a directory `d` with a file `f` in it, and a
/// file `big` written a mebibyte past what group 0 has free, so that its
/// inode is in group 0, as every inode here is, and some of its blocks
/// are not. `big` is linked into `d` as well. Unmounted, so its log is
/// clean.
fn populated() -> Arc<Sparse> {
    use fs_xfs::alloc_btree::{walk_from_agf, Order};
    let dev = volume();
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    let sb = fs.superblock().clone();
    let root = sb.rootino;
    let (d, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
    let (f, _) = fs.create_file(d, b"f", 0o100644).expect("create");
    fs.write(f, 0, &[1u8; 5000]).expect("write");
    let (big, _) = fs.create_file(root, b"big", 0o100644).expect("create");
    let bs = u64::from(sb.blocksize);
    let agf = fs.read_agf(0).expect("the AGF");
    let read = |b: u32| -> fs_xfs::Result<Vec<u8>> {
        let mut raw = vec![0u8; bs as usize];
        dev.read_at(u64::from(b) * bs, &mut raw)?;
        Ok(raw)
    };
    let free: u64 = walk_from_agf(&sb, &agf, Order::ByBlock, read)
        .expect("free space")
        .iter()
        .map(|e| u64::from(e.blockcount))
        .sum();
    fs.write(big, 0, &vec![2u8; (free * bs + 1024 * 1024) as usize])
        .expect("a write larger than group 0");
    fs.link(big, d, b"again").expect("link");
    fs.unmount().expect("unmount");

    let fs = mount(&dev);
    let (inode, raw) = fs.read_inode_raw(big).expect("big");
    let groups: BTreeSet<u32> = fs
        .data_extents(&inode, &raw)
        .expect("big's extents")
        .iter()
        .map(|e| sb.split_fsblock(e.startblock).0)
        .collect();
    assert!(
        groups.iter().any(|&g| g != 0),
        "big was meant to have blocks outside group 0, and is in groups {groups:?}"
    );
    let report = fs_xfs::check::check(&fs);
    assert!(
        report.is_clean(),
        "the populated volume has findings:\n{}",
        listed(&report)
    );
    dev
}

/// Group 0's inode btree cannot be walked, so no inode is known to be
/// allocated. The root, `d`, `f` and `big` are each read on their own
/// and walked: the tree is reached, every link counted, and `big`'s
/// blocks outside group 0 are claimed.
#[test]
fn an_entry_into_a_group_whose_inode_btree_was_not_read_is_walked() {
    let dev = populated();
    let root = mount(&dev).read_agi(0).expect("the AGI").root;
    damage_block(&dev, 0, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, 0, "inode");
    let wrong: Vec<String> = report
        .findings
        .iter()
        .filter(|f| {
            matches!(
                f.code,
                Code::DirRoot
                    | Code::DirEntryTarget
                    | Code::DirUnreached
                    | Code::InodeNlink
                    | Code::Lost
            )
        })
        .map(|f| format!("{} ag {:?}", f.code.as_str(), f.location.ag))
        .collect();
    assert!(
        wrong.is_empty(),
        "inodes in a group whose inode btree was not read are judged as if none were \
         allocated: {wrong:?}\n{}",
        listed(&report)
    );
    assert_eq!(
        report.directories,
        2,
        "the walk did not go through the root and d:\n{}",
        listed(&report)
    );
}

/// A fresh volume whose user quota inode is `q` in the root, a regular
/// file or, when `directory`, a directory: user accounting on and nothing
/// marked checked, so no usage is compared, and `q` has no blocks, so no
/// quota record is read. Its inode is in group 0, beside the root, as a
/// quota inode `mkfs.xfs` makes is. Unmounted, so its log is clean.
fn quota_volume(directory: bool) -> Arc<Sparse> {
    let dev = volume();
    let fs = Filesystem::mount_rw(dev.clone() as Arc<dyn BlockDevice>).expect("mount_rw");
    let root = fs.superblock().rootino;
    let (q, _) = if directory {
        fs.create_directory(root, b"q", 0o040700).expect("mkdir")
    } else {
        fs.create_file(root, b"q", 0o100600).expect("create")
    };
    fs.unmount().expect("unmount");
    let mut sb = mount(&dev).superblock().clone();
    sb.uquotino = q;
    sb.qflags = 0x1;
    let mut sector = vec![0u8; usize::from(sb.sectsize)];
    dev.read_at(0, &mut sector).expect("the superblock");
    fs_xfs::super_write::apply(&mut sector, &sb).expect("the quota fields");
    dev.write_at(0, &sector).expect("the superblock");
    if !directory {
        let report = fs_xfs::check::check(&mount(&dev));
        assert!(
            report.is_clean(),
            "the volume with a quota inode has findings:\n{}",
            listed(&report)
        );
    }
    dev
}

/// What `report` says under `quota.inode`.
fn quota_inode_findings(report: &Report) -> Vec<&str> {
    report
        .findings
        .iter()
        .filter(|f| f.code == Code::QuotaInode)
        .map(|f| f.what.as_str())
        .collect()
}

/// Group 0's inode btree cannot be walked, so nothing says the user
/// quota inode is allocated, and the quota check runs before the walk
/// that would read it on its own (#507). It is read on its own there
/// instead, and is not reported missing.
#[test]
fn a_quota_inode_in_a_group_whose_inode_btree_was_not_read_is_not_missing() {
    let dev = quota_volume(false);
    let root = mount(&dev).read_agi(0).expect("the AGI").root;
    damage_block(&dev, 0, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, 0, "inode");
    let quota = quota_inode_findings(&report);
    assert!(
        quota.is_empty(),
        "a quota inode in a group whose inode btree was not read is reported: {quota:?}\n{}",
        listed(&report)
    );
}

/// The same, with a directory as the quota inode: read on its own, it is
/// checked like one the inode btree listed, and is reported as no quota
/// file rather than as missing.
#[test]
fn a_quota_inode_in_a_group_whose_inode_btree_was_not_read_is_checked() {
    let dev = quota_volume(true);
    let root = mount(&dev).read_agi(0).expect("the AGI").root;
    damage_block(&dev, 0, root);
    let report = fs_xfs::check::check(&mount(&dev));
    assert_unwalked(&report, 0, "inode");
    let quota = quota_inode_findings(&report);
    assert!(
        quota.len() == 1 && quota[0].contains("has an invalid type"),
        "a directory as the quota inode, in a group whose inode btree was not read, is not \
         reported as an invalid quota file: {quota:?}\n{}",
        listed(&report)
    );
}
