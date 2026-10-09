//! Node-form directories this driver grows, edits and shrinks are the
//! ones the Linux kernel lists and edits, and `xfs_repair` accepts; and
//! node-form directories the kernel made are ones this driver reads and
//! edits (#367).
//!
//! Two volumes, both made by `mkfs.xfs` in the harness guest:
//!
//! - `-b size=1024 -n size=1024`, where a leaf or node block holds 120
//!   records, so a two-level index arrives after some thousands of names.
//!   The driver links names to one file into `d` until its root is a node
//!   of level two, then takes out every third name, which makes leaves
//!   under-full and joins them, renames some in place, moves some to `e`,
//!   and makes a few new files there. The kernel mounts the result, lists
//!   both directories and looks names up, then adds names of its own and
//!   takes some of the driver's away, in the tree the driver built, and
//!   lists again; `xfs_db` must read the root as a level-two node.
//! - `-b size=1024` alone, so directory blocks are 4 KiB over 1 KiB
//!   filesystem blocks and every pointer in the index counts filesystem
//!   blocks, four to a directory block. The kernel makes `k` with
//!   thousands of names; the driver must find every one, then adds,
//!   removes and renames names in it, and the kernel lists the result.
//!
//! `xfs_repair -n` must call each volume clean: every leaf's order and
//! count, every node's hashes, every free block's bests, every block's
//! ownership and the map that holds them.
//!
//! Listings are compared by count and SHA-256 of the sorted names, so the
//! output stays within its budget at tens of thousands of names.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

const SUITE: &str = "node_directories_oracle";

fn name(i: usize) -> Vec<u8> {
    format!("a-name-in-a-node-form-directory-{i:05}").into_bytes()
}

/// `count sha256` of names sorted bytewise, one per line, as the guest
/// computes it with `LC_ALL=C sort | sha256sum`.
fn digest<'a>(names: impl Iterator<Item = &'a Vec<u8>>) -> String {
    let sorted: BTreeSet<&Vec<u8>> = names.collect();
    let mut h = Sha256::new();
    for n in &sorted {
        h.update(n);
        h.update(b"\n");
    }
    format!("{} {:x}", sorted.len(), h.finalize())
}

/// The guest's line for directory `dir`: count and digest of its names.
fn listed<'a>(out: &'a str, key: &str) -> &'a str {
    out.lines()
        .find_map(|l| l.trim().strip_prefix(&format!("{key} ")))
        .unwrap_or_else(|| panic!("no {key} line:\n{out}"))
        .trim()
}

/// The level of `dir`'s index root, read through this driver.
fn root_level(fs: &Filesystem, dir: u64) -> Option<u16> {
    let (inode, raw) = fs.read_inode_raw(dir).expect("directory");
    if format!("{:?}", inode.format) == "Local" {
        return None;
    }
    let bs = u64::from(fs.superblock().blocksize);
    let leaf = (1u64 << 35) / bs;
    let e = fs
        .data_extents(&inode, &raw)
        .expect("extents")
        .into_iter()
        .find(|e| e.startoff <= leaf && leaf < e.startoff + e.blockcount)?;
    let mut block = vec![0u8; 64];
    fs.device()
        .read_at(
            fs.superblock()
                .fsblock_offset(e.startblock + (leaf - e.startoff)),
            &mut block,
        )
        .expect("read the root");
    (u16::from_be_bytes([block[8], block[9]]) == 0x3ebe)
        .then(|| u16::from_be_bytes([block[58], block[59]]))
}

/// One past the highest `name(i)` ever given: the names linked, whatever
/// has been done to them since, are all below it.
fn grown_to(d: &BTreeMap<Vec<u8>, u64>) -> usize {
    d.keys()
        .filter_map(|n| n.strip_prefix(b"a-name-in-a-node-form-directory-"))
        .filter_map(|i| std::str::from_utf8(i).ok()?.parse::<usize>().ok())
        .max()
        .map_or(0, |i| i + 1)
}

/// A guest shell fragment printing `KEY count sha` for directory `$m/dir`.
fn list(key: &str, dir: &str) -> String {
    format!(
        r#"echo "{key} $(ls -1f "$m/{dir}" | grep -vx '\.\.\?' | LC_ALL=C sort | wc -l) $(ls -1f "$m/{dir}" | grep -vx '\.\.\?' | LC_ALL=C sort | sha256sum | cut -d' ' -f1)""#
    )
}

#[test]
fn a_two_level_index_the_driver_builds_is_one_the_kernel_reads_and_edits() {
    let volume = scratch::Volume::empty(SUITE, "node1k.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -b size=1024 -n size=1024 {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        mkdir "$m/d" "$m/e"
        : > "$m/f"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );

    let mut d: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let mut e: BTreeMap<Vec<u8>, u64> = BTreeMap::new();
    let dir_ino;
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = |p: &str| fs.lookup_path(p).expect(p).ino;
        let (dir, other, file) = (ino("/d"), ino("/e"), ino("/f"));
        dir_ino = dir;
        let mut i = 0;
        while root_level(&fs, dir) != Some(2) {
            assert!(i < 40_000, "{i} names and the root is not a level-2 node");
            fs.link(file, dir, &name(i))
                .unwrap_or_else(|err| panic!("link {i}: {err:?}"));
            d.insert(name(i), file);
            i += 1;
        }
        let grown = i;
        // Around the joins: every third name out.
        for i in (0..grown).step_by(3) {
            fs.unlink_file(dir, &name(i))
                .unwrap_or_else(|err| panic!("unlink {i}: {err:?}"));
            d.remove(&name(i));
        }
        for i in (1..grown).step_by(37) {
            let new = format!("renamed-{i}-to-a-name-of-another-length").into_bytes();
            fs.rename_in_directory(dir, &name(i), &new)
                .unwrap_or_else(|err| panic!("rename {i}: {err:?}"));
            d.remove(&name(i));
            d.insert(new, file);
        }
        for i in (2..grown).step_by(101) {
            fs.rename(dir, &name(i), other, &name(i))
                .unwrap_or_else(|err| panic!("move {i}: {err:?}"));
            d.remove(&name(i));
            e.insert(name(i), file);
        }
        for i in 0..20 {
            let n = format!("made-by-the-driver-{i}").into_bytes();
            let (made, _) = fs.create_file(dir, &n, 0o100644).expect("create");
            d.insert(n, made);
        }
        assert_eq!(root_level(&fs, dir), Some(2), "still two levels");
    }
    // What the kernel does to the driver's tree: takes out every
    // fifteenth of the driver's names, where they are still there, and
    // adds names of its own.
    let kernel_removes: Vec<Vec<u8>> = (4..grown_to(&d))
        .step_by(15)
        .map(name)
        .filter(|n| d.contains_key(n))
        .collect();
    let samples: Vec<&Vec<u8>> = d.keys().step_by(97).collect();
    let mut stats = String::new();
    for s in &samples {
        let s = String::from_utf8_lossy(s);
        stats.push_str(&format!("echo \"INO {s} $(stat -c %i \"$m/d/{s}\")\"\n"));
    }
    let removes = format!(
        "for i in $(seq 4 15 {}); do rm -f \"$m/d/$(printf 'a-name-in-a-node-form-directory-%05d' $i)\"; done",
        grown_to(&d) - 1
    );
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {list_d}
            {list_e}
            {stats}
            {removes}
            for i in $(seq 0 299); do : > "$m/d/made-by-the-kernel-$i"; done
            {list_after}
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "LEVEL $(xfs_db -r -c 'inode {dir_ino}' -c 'dblock 33554432' -c 'print nhdr.level' "$img" 2>&1 | tr '\n' ' ')"
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo DONE
        "#,
        list_d = list("D", "d"),
        list_e = list("E", "e"),
        list_after = list("AFTER", "d"),
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, "a two-level node directory the driver built");
    assert_eq!(
        listed(&out, "D"),
        digest(d.keys()),
        "d: the kernel lists other names"
    );
    assert_eq!(
        listed(&out, "E"),
        digest(e.keys()),
        "e: the kernel lists other names"
    );
    for s in &samples {
        let s = String::from_utf8_lossy(s);
        let found = listed(&out, &format!("INO {s}"));
        let want = d[s.as_bytes()];
        assert_eq!(
            found,
            want.to_string(),
            "{s}: the kernel looks up another inode"
        );
    }
    let mut after = d.clone();
    for r in &kernel_removes {
        after.remove(r);
    }
    for i in 0..300 {
        after.insert(format!("made-by-the-kernel-{i}").into_bytes(), 0);
    }
    assert_eq!(
        listed(&out, "AFTER"),
        digest(after.keys()),
        "the kernel's own edits to the driver's tree list other names"
    );
    assert!(
        listed(&out, "LEVEL").contains("nhdr.level = 2"),
        "xfs_db does not read a level-2 root:\n{out}"
    );
}

#[test]
fn a_node_directory_the_kernel_made_over_small_blocks_is_read_and_edited() {
    let volume = scratch::Volume::empty(SUITE, "node4k.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -b size=1024 {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        mkdir "$m/k"
        (cd "$m/k" && seq -f 'kernel-made-name-in-a-node-directory-%05g' 0 2999 | xargs touch)
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let kname = |i: usize| format!("kernel-made-name-in-a-node-directory-{i:05}").into_bytes();

    let mut k: BTreeSet<Vec<u8>> = (0..3000).map(kname).collect();
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let dir = fs.lookup_path("/k").expect("k").ino;
        assert!(
            root_level(&fs, dir).is_some(),
            "3000 names under 4 KiB directory blocks are not in node form"
        );
        // Every name the kernel made is found through the index.
        let (inode, raw) = fs.read_inode_raw(dir).expect("k");
        let mut first = 0;
        for i in 0..3000 {
            let found = fs
                .lookup(&inode, &raw, &kname(i))
                .unwrap_or_else(|err| panic!("lookup {i}: {err:?}"));
            if i == 0 {
                first = found.ino;
            }
        }
        for i in 0..1000 {
            let n = format!("linked-by-the-driver-{i}").into_bytes();
            fs.link(first, dir, &n)
                .unwrap_or_else(|err| panic!("link {i}: {err:?}"));
            k.insert(n);
        }
        for i in (1..3000).step_by(2) {
            fs.unlink_file(dir, &kname(i))
                .unwrap_or_else(|err| panic!("unlink {i}: {err:?}"));
            k.remove(&kname(i));
        }
        for i in (2..3000).step_by(30) {
            let new = format!("renamed-by-the-driver-{i}").into_bytes();
            fs.rename_in_directory(dir, &kname(i), &new)
                .unwrap_or_else(|err| panic!("rename {i}: {err:?}"));
            k.remove(&kname(i));
            k.insert(new);
        }
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {list_k}
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo DONE
        "#,
        list_k = list("K", "k"),
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, "a kernel-made node directory the driver edited");
    assert_eq!(
        listed(&out, "K"),
        digest(k.iter()),
        "k: the kernel lists other names"
    );
}
