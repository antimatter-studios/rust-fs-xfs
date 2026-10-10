//! Each repair rule starts from one xfs_db fault in a kernel-built volume.
//! Idempotence is byte-for-byte, and the kernel namespace, link counts,
//! file contents and xfs_repair -n must all agree after repair (#393).

mod cli_support;
mod common;

use cli_support::{stderr, tool};
use common::{kernel_run, oracle, repair, scratch};
use fs_core::FileDevice;
use fs_xfs::{
    inode_btree::{self, Which},
    Filesystem,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::sync::Arc;

const SUITE: &str = "inode_repair_oracle";

fn build(tag: &str, options: &str) -> scratch::Volume {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-{tag}.img", std::process::id()),
        400 << 20,
    );
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        mkfs.xfs -f -q -d agcount=2 {options} {image}
        m=$(mktemp -d)
        mount -o loop,nouuid {image} "$m"
        mkdir -p "$m/dir/sub"
        printf 'inode repair payload\n' > "$m/file"
        ln "$m/file" "$m/alias"
        printf 'another owner\n' > "$m/other"
        ln -s file "$m/sym"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        {repair}
        echo BUILT
        echo DONE
    "#,
        repair = repair::script(&image)
    ));
    repair::assert_agreed(&out, "the kernel-built inode repair fixture");
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    assert!(out.contains("BUILT"));
    volume
}

fn fs(volume: &scratch::Volume) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open(volume.path()).unwrap())).unwrap()
}

fn inode(volume: &scratch::Volume, name: &[u8]) -> u64 {
    let fs = fs(volume);
    let (root, raw) = fs.read_inode_raw(fs.superblock().rootino).unwrap();
    fs.lookup(&root, &raw, name).unwrap().ino
}

fn hash(volume: &scratch::Volume) -> Vec<u8> {
    let mut input = std::fs::File::open(volume.path()).unwrap();
    let mut digest = Sha256::new();
    let mut buf = vec![0; 1 << 20];
    loop {
        let n = input.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    digest.finalize().to_vec()
}

fn damage(volume: &scratch::Volume, commands: &[String]) {
    let before = hash(volume);
    let mut db = oracle("xfs_db").arg("-x");
    for command in commands {
        db = db.args(["-c", command]);
    }
    let out = db.arg(volume.path().to_str().unwrap()).output();
    assert!(out.ok(), "xfs_db failed: {}{}", out.stdout, out.stderr);
    assert_ne!(
        hash(volume),
        before,
        "xfs_db did not write: {commands:?}: {}{}",
        out.stdout,
        out.stderr
    );
    assert!(
        !out.stdout.contains("not found") && !out.stdout.contains("bad field"),
        "xfs_db rejected a field: {}",
        out.stdout
    );
    assert!(
        !fs_xfs::check::check(&fs(volume)).is_clean(),
        "the fault was not injected: {commands:?}"
    );
    let out = oracle("xfs_repair")
        .args(["-n", volume.path().to_str().unwrap()])
        .output();
    let report = out.repair_report();
    assert!(
        !common::repair::was_blind(&report),
        "the fault fixture has an unreplayed log: {report}"
    );
    assert!(
        !out.ok(),
        "xfs_repair -n found no fault for {commands:?}: {report}"
    );
}

fn repaired(volume: &scratch::Volume, case: &str) {
    let out = tool("fsck.xfs")
        .arg("-y")
        .arg(volume.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{case}: {}", stderr(&out));
    assert!(fs_xfs::check::check(&fs(volume)).is_clean(), "{case}");
    let first = hash(volume);
    let out = tool("fsck.xfs")
        .arg("-y")
        .arg(volume.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{case}: {}", stderr(&out));
    assert_eq!(
        hash(volume),
        first,
        "{case}: second repair wrote to the volume"
    );
    let image = volume.guest();
    // Ask the repair oracle before the kernel touches the repaired image too:
    // a kernel that silently fixes our output cannot make this test pass.
    let out = kernel_run(&format!(
        r#"
        {repair}
        m=$(mktemp -d)
        mount -o loop,nouuid {image} "$m"
        echo "ROOT_LINKS=$(stat -c %h "$m")"
        echo "DIR_LINKS=$(stat -c %h "$m/dir")"
        echo "SUB_LINKS=$(stat -c %h "$m/dir/sub")"
        echo "FILE_LINKS=$(stat -c %h "$m/file")"
        echo "ALIAS_LINKS=$(stat -c %h "$m/alias")"
        echo "SYMLINK_LINKS=$(stat -c %h "$m/sym")"
        test "$(stat -c %i "$m/file")" = "$(stat -c %i "$m/alias")"
        test "$(cat "$m/file")" = 'inode repair payload'
        test "$(cat "$m/other")" = 'another owner'
        test "$(readlink "$m/sym")" = file
        echo "NAMES=$(find "$m" -mindepth 1 -printf '%P\n' | sort | tr '\n' ',')"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
    "#,
        repair = repair::script(&image)
    ));
    repair::assert_agreed(&out, case);
    assert!(!out.contains("UMOUNT_FAILED"), "{case}: {out}");
    for expected in [
        "ROOT_LINKS=3",
        "DIR_LINKS=3",
        "SUB_LINKS=2",
        "FILE_LINKS=2",
        "ALIAS_LINKS=2",
        "SYMLINK_LINKS=1",
        "NAMES=alias,dir,dir/sub,file,other,sym,",
    ] {
        assert!(out.contains(expected), "{case}: missing {expected}: {out}");
    }
}

#[test]
fn each_link_count_rule_has_an_independent_single_fault() {
    let source = build("links", "-m rmapbt=0");
    let cases = [
        ("root", fs(&source).superblock().rootino),
        ("directory", inode(&source, b"dir")),
        ("hard-linked-file", inode(&source, b"file")),
        ("symlink", inode(&source, b"sym")),
    ];
    for (case, ino) in cases {
        let volume = scratch::Volume::copy_of(
            SUITE,
            source.path(),
            &format!("{}-{case}.img", std::process::id()),
        );
        damage(
            &volume,
            &[format!("inode {ino}"), "write -d core.nlinkv2 19".into()],
        );
        repaired(&volume, case);
    }
}

#[test]
fn allocation_bits_counts_and_finobt_are_repaired_on_sparse_and_plain_chunks() {
    for (geometry, opts) in [
        ("rmap", "-m rmapbt=1"),
        ("sparse", "-m rmapbt=0"),
        ("plain", "-m rmapbt=0 -i sparse=0"),
        ("no-finobt", "-m rmapbt=0,finobt=0 -i sparse=0"),
    ] {
        let source = build(geometry, opts);
        let fs = fs(&source);
        let sb = fs.superblock();
        let agi = fs.read_agi(0).unwrap();
        let chunks = inode_btree::walk_from_agi(sb, &agi, Which::All, |b| {
            let mut raw = vec![0; sb.blocksize as usize];
            fs.device()
                .read_at(u64::from(b) * u64::from(sb.blocksize), &mut raw)?;
            Ok(raw)
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            chunks.len(),
            1,
            "the single-fault fixture must have one chunk"
        );
        let chunk = chunks[0];
        let file_bit = inode(&source, b"file") - u64::from(chunk.startino);
        assert!(file_bit < 64);
        let mut cases = vec![
            (
                "referenced-inode-marked-free",
                "root",
                "free",
                format!("0x{:x}", chunk.free | (1u64 << file_bit)),
            ),
            (
                "unused-inode-marked-allocated",
                "root",
                "free",
                format!("0x{:x}", chunk.free & !(1u64 << 63)),
            ),
            ("inobt-freecount", "root", "freecount", "0".into()),
        ];
        if sb.has_finobt() {
            cases.push((
                "finobt-free-mask",
                "free_root",
                "free",
                format!("0x{:x}", chunk.free & !(1u64 << 63)),
            ));
            cases.push(("finobt-freecount", "free_root", "freecount", "0".into()));
            cases.push(("finobt-membership", "free_root", "numrecs", "0".into()));
        }
        for (case, tree, field, value) in cases {
            let volume = scratch::Volume::copy_of(
                SUITE,
                source.path(),
                &format!("{}-{geometry}-{case}.img", std::process::id()),
            );
            damage(
                &volume,
                &[
                    "agi 0".into(),
                    format!("addr {tree}"),
                    if field == "numrecs" {
                        format!("write -d numrecs {value}")
                    } else {
                        format!("write -d recs[1].{field} {value}")
                    },
                ],
            );
            repaired(&volume, &format!("{geometry}/{case}"));
        }
        if geometry == "sparse" {
            // One allocation mistake, consistently mirrored in both trees
            // and their counters: an unused slot is called allocated.
            let volume = scratch::Volume::copy_of(
                SUITE,
                source.path(),
                &format!("{}-mirrored-allocation.img", std::process::id()),
            );
            let free = chunk.free & !(1u64 << 63);
            let mut commands = Vec::new();
            for tree in ["root", "free_root"] {
                commands.extend([
                    "agi 0".into(),
                    format!("addr {tree}"),
                    format!("write -d recs[1].free 0x{free:x}"),
                    format!("write -d recs[1].freecount {}", chunk.freecount - 1),
                ]);
            }
            commands.extend([
                "agi 0".into(),
                format!("write -d freecount {}", agi.freecount - 1),
                "sb 0".into(),
                format!("write -d ifree {}", sb.ifree - 1),
            ]);
            damage(&volume, &commands);
            repaired(&volume, "mirrored single-inode allocation mistake");
        }
    }
}

#[test]
fn link_census_includes_every_leaf_directory_data_block() {
    let volume = build("leaf-directory", "-m rmapbt=1");
    let image = volume.guest();
    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        mount -o loop,nouuid {image} "$m"
        for i in $(seq 0 79); do ln "$m/file" "$m/dir/entry-$i-$(printf '%080d' 0)"; done
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        {repair}
        echo DONE
    "#,
        repair = repair::script(&image)
    ));
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    repair::assert_agreed(&out, "leaf directory fixture");
    let source = fs(&volume);
    assert_ne!(
        source.read_inode(inode(&volume, b"dir")).unwrap().format,
        fs_xfs::inode::Format::Local
    );
    damage(
        &volume,
        &[
            format!("inode {}", inode(&volume, b"file")),
            "write -d core.nlinkv2 19".into(),
        ],
    );
    let out = tool("fsck.xfs")
        .arg("-y")
        .arg(volume.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(fs_xfs::check::check(&fs(&volume)).is_clean());
    let before = hash(&volume);
    let out = tool("fsck.xfs")
        .arg("-y")
        .arg(volume.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(hash(&volume), before);
    let out = kernel_run(&format!(
        r#"
        {repair}
        m=$(mktemp -d)
        mount -o loop,nouuid {image} "$m"
        test "$(stat -c %h "$m/file")" = 82
        test "$(stat -c %h "$m/dir")" = 3
        test "$(find "$m/dir" -mindepth 1 -maxdepth 1 | wc -l)" = 81
        for i in $(seq 0 79); do
            test "$(stat -c %i "$m/dir/entry-$i-$(printf '%080d' 0)")" = "$(stat -c %i "$m/file")"
        done
        test "$(cat "$m/file")" = 'inode repair payload'
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo LEAF_NAMESPACE_OK
        echo DONE
    "#,
        repair = repair::script(&image)
    ));
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    repair::assert_agreed(&out, "leaf directory repaired links");
    assert!(out.contains("LEAF_NAMESPACE_OK"), "{out}");
}

#[test]
fn ambiguous_inode_ownership_refuses_the_entire_plan_unchanged() {
    let source = build("ambiguous", "-m rmapbt=0");
    let root = fs(&source).superblock().rootino;
    let file = inode(&source, b"file");
    let other = inode(&source, b"other");
    let dir = inode(&source, b"dir");
    let source_fs = fs(&source);
    let (root_inode, raw) = source_fs.read_inode_raw(root).unwrap();
    let other_index = source_fs
        .read_dir(&root_inode, &raw)
        .unwrap()
        .iter()
        .position(|e| e.name == b"other")
        .unwrap();
    for (case, commands) in [
        (
            "orphan",
            vec![
                format!("inode {root}"),
                format!("write -d u3.sfdir3.list[{other_index}].inumber.i4 {file}"),
            ],
        ),
        (
            "wrong-parent",
            vec![
                format!("inode {dir}"),
                format!("write -d u3.sfdir3.hdr.parent.i4 {dir}"),
            ],
        ),
        (
            "inode-identity",
            vec![format!("inode {file}"), format!("write -d v3.ino {other}")],
        ),
    ] {
        let volume = scratch::Volume::copy_of(
            SUITE,
            source.path(),
            &format!("{}-{case}.img", std::process::id()),
        );
        // This supported fault ensures refusal does not apply an earlier
        // planned correction before discovering the ownership ambiguity.
        let mut faults = vec![format!("inode {root}"), "write -d core.nlinkv2 19".into()];
        faults.extend(commands);
        damage(&volume, &faults);
        match case {
            "orphan" => assert_eq!(inode(&volume, b"other"), file),
            "wrong-parent" => {
                let fs = fs(&volume);
                let (inode, raw) = fs.read_inode_raw(dir).unwrap();
                let (start, end) = inode.data_fork_range(fs.superblock().inodesize as usize);
                assert_eq!(
                    fs_xfs::dir::read_short_form(&inode, &raw[start..end], fs.superblock())
                        .unwrap()
                        .parent_ino,
                    dir
                );
            }
            _ => assert!(fs(&volume).read_inode(file).is_err()),
        }
        let before = hash(&volume);
        let out = tool("fsck.xfs")
            .arg("-y")
            .arg(volume.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(4), "{case}: {}", stderr(&out));
        assert_eq!(hash(&volume), before, "{case}: refusal changed the device");
    }
}
