//! Directory repairs are judged by xfsprogs and the real kernel (#394).

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, oracle, scratch};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;

fn kernel_contents(image: &str) -> String {
    let out = kernel_run(&format!(
        r#"
        set -eu
        img=$(mktemp /tmp/issue394-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        mount -o loop,ro,nouuid,norecovery "$img" "$m"
        (cd "$m"; find . -printf 'ENTRY %p %y %i %n %l\n' | sort;
         find . -type f -print0 | sort -z | xargs -0 sha256sum)
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        rm -f "$img"
        echo DONE
        "#,
        image = guest_quote(image),
    ));
    out.lines()
        .filter(|line| line.starts_with("ENTRY ") || line.contains("  ./"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn an_unambiguous_short_form_entry_type_is_repaired() {
    let copy = scratch::Volume::copy_of(
        "cli_directory_repair",
        &fixture("xfsdata-default.img"),
        "short-form-type.img",
    );
    let image = copy.path().to_str().unwrap();
    let before = kernel_contents(image);
    assert!(!before.is_empty(), "the kernel read no fixture contents");
    let root = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "p rootino", image])
        .output()
        .stdout
        .split('=')
        .nth(1)
        .expect("xfs_db rootino")
        .trim()
        .to_string();
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            &format!("inode {root}"),
            "-c",
            "write -d u3.sfdir3.list[0].filetype 2",
            image,
        ])
        .output();
    assert!(edit.ok(), "{}{}", edit.stdout, edit.stderr);
    let damaged = oracle("xfs_repair").args(["-n", image]).output();
    let damaged_report = damaged.repair_report();
    assert!(!common::repair::was_blind(&damaged_report));
    assert!(!damaged.ok(), "fault was not detected: {damaged_report}");

    let repaired = tool("fsck.xfs")
        .args(["-y", "--text", image])
        .output()
        .unwrap();
    assert_eq!(
        repaired.status.code(),
        Some(1),
        "fsck must report corrected errors: {}{}",
        stdout(&repaired),
        stderr(&repaired)
    );
    let report = oracle("xfs_repair")
        .args(["-n", image])
        .output()
        .repair_report();
    common::repair::assert_agreed(&report, "the repaired entry type");
    assert_eq!(kernel_contents(image), before);
}

fn image_hash(path: &std::path::Path) -> Vec<u8> {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hash = Sha256::new();
    let mut bytes = vec![0; 1024 * 1024];
    loop {
        let n = file.read(&mut bytes).unwrap();
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    hash.finalize().to_vec()
}

fn db(image: &str, commands: &[String], write: bool) -> String {
    let mut args = vec![if write {
        "-x".to_string()
    } else {
        "-r".to_string()
    }];
    for command in commands {
        args.extend(["-c".to_string(), command.clone()]);
    }
    args.push(image.to_string());
    let out = oracle("xfs_db").args(args).output();
    assert!(out.ok(), "{}{}", out.stdout, out.stderr);
    assert!(
        !out.stdout.contains("not found") && !out.stdout.contains("bad value"),
        "{}",
        out.stdout
    );
    out.stdout
}

fn value(dump: &str, key: &str) -> u64 {
    let text = dump
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key} = ")))
        .unwrap_or_else(|| panic!("{key} missing from {dump}"));
    let text = text.split_whitespace().next().unwrap();
    if let Some(hex) = text.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).unwrap()
    } else {
        text.parse().unwrap()
    }
}

fn fault_matrix(dirblock: usize) {
    let suite = format!("cli_directory_repair_{dirblock}");
    let base = scratch::Volume::empty(&suite, "base.img", 400 * 1024 * 1024);
    let image = base.path().to_str().unwrap();
    let built = kernel_run(&format!(
        r#"
        set -eu
        mkfs.xfs -f -q -m rmapbt=0 -n size={dirblock} {image}
        m=$(mktemp -d)
        mount -o loop {image} "$m"
        for shape in sf block leaf node; do
            mkdir "$m/$shape"
            case "$shape" in sf) n=2;; block) n=40;; leaf) n=300;; node) n=2400;; esac
            for i in $(seq 1 "$n"); do
                printf 'contents %s\n' "$i" > "$m/$shape/entry-$(printf %05d "$i")-abcdefgh"
            done
            printf 'INODE %s %s\n' "$shape" "$(stat -c %i "$m/$shape")"
        done
        # Deletions leave real stale index slots, including in later leaves.
        for i in $(seq 100 100 2400); do
            rm "$m/node/entry-$(printf %05d "$i")-abcdefgh"
        done
        ln "$m/sf/entry-00001-abcdefgh" "$m/another-link"
        ln -s sf/entry-00001-abcdefgh "$m/symlink"
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        echo DONE
    "#,
        image = guest_quote(image)
    ));
    let inodes: BTreeMap<_, _> = built
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("INODE ")?;
            let (name, ino) = rest.split_once(' ')?;
            Some((name.to_string(), ino.parse::<u64>().unwrap()))
        })
        .collect();
    assert_eq!(inodes.len(), 4, "{built}");
    let before = kernel_contents(image);
    let leaf_da = (1u64 << 35) / 4096;
    let node_dump = db(
        image,
        &[
            format!("inode {}", inodes["node"]),
            format!("dblock {leaf_da}"),
            "p".into(),
            // A whole node prints its entries as one array, `0:[hash,before]`;
            // a field asked for by name prints as `name = value`.
            "p nbtree[0].before".into(),
        ],
        false,
    );
    assert_eq!(value(&node_dump, "nhdr.info.hdr.magic"), 0x3ebe);
    let child = value(&node_dump, "nbtree[0].before");
    let mut faults = Vec::new();
    for write in [
        "u3.sfdir3.list[0].filetype 2",
        "u3.sfdir3.list[0].filetype 255",
        "u3.sfdir3.hdr.parent.i4 0",
    ] {
        faults.push(("sf", None, write.to_string()));
    }
    for shape in ["block", "leaf", "node"] {
        let prefix = if shape == "block" { "bu" } else { "du" };
        for write in [
            format!("{prefix}[2].filetype 2"),
            format!("{prefix}[2].tag 0"),
            format!("{prefix}[0].inumber 0"),
            format!("{prefix}[1].inumber 0"),
        ] {
            faults.push((shape, Some(0), write));
        }
    }
    for write in [
        "bleaf[2].hashval 0",
        "bleaf[2].address 0",
        "btail.stale 1",
        "bu[42].tag 0",
    ] {
        faults.push(("block", Some(0), write.to_string()));
    }
    for (shape, da) in [("leaf", leaf_da), ("node", child)] {
        for write in ["lents[2].hashval 0", "lents[2].address 0", "lhdr.stale 1"] {
            faults.push((shape, Some(da), write.to_string()));
        }
    }
    faults.push(("node", Some(leaf_da), "nbtree[0].hashval 0".to_string()));
    for (index, (shape, da, write)) in faults.iter().enumerate() {
        let copy = scratch::Volume::copy_of(&suite, base.path(), &format!("fault-{index}.img"));
        let image = copy.path().to_str().unwrap();
        let mut commands = vec![format!("inode {}", inodes[*shape])];
        if let Some(da) = da {
            commands.push(format!("dblock {da}"));
        }
        commands.push(format!("write -d {write}"));
        db(image, &commands, true);
        let damaged = oracle("xfs_repair").args(["-n", image]).output();
        let report = damaged.repair_report();
        assert!(
            !common::repair::was_blind(&report),
            "{dirblock} {shape} {write}: {report}"
        );
        assert!(
            !damaged.ok(),
            "fault not detected: {dirblock} {shape} {write}: {report}"
        );
        let hash = image_hash(copy.path());
        let read_only = tool("fsck.xfs").args(["-n", image]).output().unwrap();
        assert!(
            matches!(read_only.status.code(), Some(0 | 4)),
            "{}",
            stderr(&read_only)
        );
        assert_eq!(
            image_hash(copy.path()),
            hash,
            "check-only changed the image"
        );
        let repaired = tool("fsck.xfs")
            .args(["-p", "--text", image])
            .output()
            .unwrap();
        assert_eq!(
            repaired.status.code(),
            Some(1),
            "{dirblock} {shape} {write}: {}{}",
            stdout(&repaired),
            stderr(&repaired)
        );
        let report = oracle("xfs_repair")
            .args(["-n", image])
            .output()
            .repair_report();
        common::repair::assert_agreed(&report, &format!("{dirblock} {shape} {write}"));
        assert_eq!(kernel_contents(image), before, "{dirblock} {shape} {write}");
        let hash = image_hash(copy.path());
        let again = tool("fsck.xfs").args(["-y", image]).output().unwrap();
        assert_eq!(
            again.status.code(),
            Some(0),
            "{}{}",
            stdout(&again),
            stderr(&again)
        );
        assert_eq!(
            image_hash(copy.path()),
            hash,
            "clean repair was not idempotent"
        );
    }
    for (index, ambiguous) in [
        "u3.sfdir3.list[1].inumber.i4 0",
        "u3.sfdir3.list[1].name \"entry-00001-abcdefgh\"",
        "u3.sfdir3.hdr.count 1",
    ]
    .iter()
    .enumerate()
    {
        let copy = scratch::Volume::copy_of(&suite, base.path(), &format!("ambiguous-{index}.img"));
        let image = copy.path().to_str().unwrap();
        // A repairable fault beside an ambiguous one must not be written either.
        db(
            image,
            &[
                format!("inode {}", inodes["sf"]),
                "write -d u3.sfdir3.list[0].filetype 2".into(),
                format!("write -d {ambiguous}"),
            ],
            true,
        );
        let hash = image_hash(copy.path());
        let refused = tool("fsck.xfs")
            .args(["-y", "--text", image])
            .output()
            .unwrap();
        assert_eq!(
            refused.status.code(),
            Some(4),
            "{ambiguous}: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stdout(&refused).contains("refused"), "{}", stdout(&refused));
        assert_eq!(image_hash(copy.path()), hash, "ambiguous image was changed");
    }
    for (name, commands) in [
        (
            "multiple-parents",
            vec![
                "inode 128".to_string(),
                format!("write -d u3.sfdir3.list[1].inumber.i4 {}", inodes["sf"]),
            ],
        ),
        (
            "index-child",
            vec![
                format!("inode {}", inodes["node"]),
                format!("dblock {leaf_da}"),
                "write -d nbtree[0].before 0".to_string(),
            ],
        ),
    ] {
        let copy = scratch::Volume::copy_of(&suite, base.path(), &format!("{name}.img"));
        let image = copy.path().to_str().unwrap();
        db(
            image,
            &[
                format!("inode {}", inodes["sf"]),
                "write -d u3.sfdir3.list[0].filetype 2".into(),
            ],
            true,
        );
        db(image, &commands, true);
        let hash = image_hash(copy.path());
        let refused = tool("fsck.xfs")
            .args(["-y", "--text", image])
            .output()
            .unwrap();
        assert_eq!(
            refused.status.code(),
            Some(4),
            "{name}: {}{}",
            stdout(&refused),
            stderr(&refused)
        );
        assert!(stdout(&refused).contains("refused"), "{}", stdout(&refused));
        assert_eq!(image_hash(copy.path()), hash, "{name}: image was changed");
    }
}

#[test]
fn per_invariant_directory_faults_are_repaired_or_refused() {
    fault_matrix(4096);
}

#[test]
fn directory_blocks_larger_than_filesystem_blocks_are_repaired() {
    fault_matrix(8192);
}
