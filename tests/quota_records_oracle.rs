//! Quota checking against the kernel, xfs_quota and xfs_repair (#395).

mod common;
mod quota_support;

use common::{fixture, oracle, scratch};
use fs_core::FileDevice;
use fs_xfs::{check, Filesystem};
use std::sync::Arc;

const SUITE: &str = "quota_records_oracle";

fn build(label: &str, geometry: &str, options: &str) -> scratch::Volume {
    quota_support::build(SUITE, label, geometry, options, |s| common::kernel_run(s))
}

#[test]
fn kernel_quota_modes_are_clean_including_sparse_ids_and_zero_inode_size() {
    for &(label, geometry, options) in quota_support::CASES {
        let image = build(label, geometry, options);
        common::assert_xfs_repair_clean(image.path().to_str().unwrap(), label);
        let found = report(image.path());
        assert!(found.is_clean(), "{label}: {found:?}");
    }
}

#[test]
fn malformed_records_agree_with_repair_for_each_quota_type() {
    let source = build(
        "record-source",
        "-m crc=1,bigtime=1",
        "usrquota,grpquota,prjquota",
    );
    for (kind, id) in [("u", 17), ("g", 18), ("p", 65553)] {
        for (field, value, expected) in [
            ("diskdq.magic", "0", "bad magic"),
            ("diskdq.version", "2", "bad version"),
            ("diskdq.type", "0", "wrong quota type"),
            ("diskdq.id", "999", "ID does not match"),
            (
                "diskdq.blk_softlimit",
                "1",
                "exceeds soft limit without a grace timer",
            ),
            (
                "uuid",
                "00000000-0000-0000-0000-000000000000",
                "metadata UUID mismatch",
            ),
        ] {
            let image = scratch::Volume::copy_of(
                SUITE,
                source.path(),
                &format!("{}-{kind}-{field}.img", std::process::id()),
            );
            let edit = oracle("xfs_db")
                .args([
                    "-x",
                    "-c",
                    &format!("dquot -{kind} {id}"),
                    "-c",
                    &format!("write -d {field} {value}"),
                ])
                .arg(image.path())
                .output();
            assert!(edit.ok(), "{}{}", edit.stdout, edit.stderr);
            let repair = oracle("xfs_repair").arg("-n").arg(image.path()).output();
            assert!(
                !repair.ok(),
                "{kind} {field}: reference accepted malformed dquot:\n{}",
                repair.repair_report()
            );
            let found = report(image.path());
            assert!(
                found.findings.iter().any(|f| f.what.contains(expected)),
                "{kind} {field}: {found:?}"
            );
            assert_eq!(
                found.findings,
                report(image.path()).findings,
                "findings must be stable"
            );
        }
    }
}

#[test]
fn counters_are_cross_checked_and_unchecked_accounting_is_not_called_corrupt() {
    let source = build(
        "counter-source",
        "-m crc=1,bigtime=1",
        "usrquota,grpquota,prjquota",
    );
    for (kind, id, name, checked) in [
        ("u", 17, "user", 0x4),
        ("g", 18, "group", 0x100),
        ("p", 65553, "project", 0x400),
    ] {
        for (field, what) in [
            ("bcount", "blocks"),
            ("icount", "inodes"),
            ("rtbcount", "realtime blocks"),
        ] {
            let image = scratch::Volume::copy_of(
                SUITE,
                source.path(),
                &format!("{}-{kind}-{field}.img", std::process::id()),
            );
            let edit = oracle("xfs_db")
                .args([
                    "-x",
                    "-c",
                    &format!("dquot -{kind} {id}"),
                    "-c",
                    &format!("write -d diskdq.{field} 9"),
                ])
                .arg(image.path())
                .output();
            assert!(edit.ok(), "{}{}", edit.stdout, edit.stderr);
            let found = report(image.path());
            assert!(
                found
                    .findings
                    .iter()
                    .any(|f| f.what.starts_with(&format!("{name} quota inode"))
                        && f.what.contains(&format!("record {id} counts 9 {what};"))),
                "{kind} {field}: {found:?}"
            );
            assert_eq!(found.findings, report(image.path()).findings);
            // Without CHKD, mount-time quotacheck is supposed to rebuild
            // these stale counters; it is not evidence of corruption.
            let fs = Filesystem::mount(Arc::new(FileDevice::open(image.path()).unwrap())).unwrap();
            let flags = fs.superblock().qflags & !checked;
            drop(fs);
            let edit = oracle("xfs_db")
                .args([
                    "-x",
                    "-c",
                    "sb 0",
                    "-c",
                    &format!("write -d qflags {flags}"),
                ])
                .arg(image.path())
                .output();
            assert!(edit.ok());
            assert!(
                report(image.path()).is_clean(),
                "unchecked {kind}: {:?}",
                report(image.path())
            );
            // The independent kernel quotacheck must rebuild precisely the
            // usage our inode walk found, and xfs_quota observes that result.
            common::kernel_run(&format!(
                r#"set -euo pipefail
d=$(mktemp -d)
mounted=0
cleanup() {{
    if [ "$mounted" = 1 ]; then umount "$d" || {{ echo UMOUNT_FAILED >&2; exit 1; }}; fi
    rmdir "$d"
}}
trap cleanup EXIT
mount -o loop,usrquota,grpquota,prjquota {image} "$d"
mounted=1
xfs_quota -x -c 'report -{kind} -n -b -i' "$d"
sync
umount "$d" || {{ echo UMOUNT_FAILED >&2; exit 1; }}
mounted=0
"#,
                image = common::guest_quote(&image.guest())
            ));
            let expected = match field {
                "bcount" => 4,
                "icount" if kind == "p" => 2,
                "icount" => 1,
                _ => 0,
            };
            let rebuilt = oracle("xfs_db")
                .args([
                    "-c",
                    &format!("dquot -{kind} {id}"),
                    "-c",
                    &format!("print diskdq.{field}"),
                ])
                .arg(image.path())
                .output();
            assert!(rebuilt.ok(), "{}{}", rebuilt.stdout, rebuilt.stderr);
            assert_eq!(
                rebuilt.stdout.trim(),
                format!("diskdq.{field} = {expected}"),
                "kernel quotacheck disagrees with the checker"
            );
            assert!(report(image.path()).is_clean(), "kernel rebuilt {kind}");
        }
    }
}

#[test]
fn enabled_quota_requires_an_allocated_inode() {
    use std::io::{Read, Seek, SeekFrom, Write};
    let image = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-default.img"),
        &format!("{}-missing.img", std::process::id()),
    );
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(image.path())
        .unwrap();
    let mut sector = [0u8; 512];
    file.read_exact(&mut sector).unwrap();
    sector[160..168].copy_from_slice(&0u64.to_be_bytes());
    sector[176..178].copy_from_slice(&5u16.to_be_bytes());
    sector[224..228].fill(0);
    let crc = crc32c::crc32c(&sector);
    sector[224..228].copy_from_slice(&crc.to_le_bytes());
    file.seek(SeekFrom::Start(0)).unwrap();
    file.write_all(&sector).unwrap();
    drop(file);
    let found = report(image.path());
    assert!(
        found
            .findings
            .iter()
            .any(|f| f.what.contains("user quota inode")),
        "missing quota inode was missed: {found:?}"
    );
}

fn report(path: &std::path::Path) -> check::Report {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(path).unwrap())).unwrap();
    check::check(&fs)
}

#[test]
fn a_quota_inode_cannot_be_a_directory() {
    let image = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfs-default.img"),
        &format!("{}-directory.img", std::process::id()),
    );
    let fs = Filesystem::mount(Arc::new(FileDevice::open(image.path()).unwrap())).unwrap();
    let root = fs.superblock().rootino;
    drop(fs);
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            "sb 0",
            "-c",
            &format!("write -d uquotino {root}"),
            "-c",
            "write -d qflags 5",
        ])
        .arg(image.path())
        .output();
    assert!(edit.ok(), "{}{}", edit.stdout, edit.stderr);
    let repair = oracle("xfs_repair")
        .arg("-n")
        .arg(image.path())
        .output()
        .repair_report();
    assert!(
        repair.contains("quota"),
        "xfs_repair must identify the bad quota inode:\n{repair}"
    );
    let found = report(image.path());
    assert!(
        found
            .findings
            .iter()
            .any(|f| f.what.contains("user quota inode")),
        "quota inode damage was missed: {found:?}"
    );
}
