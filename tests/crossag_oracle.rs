//! Freeing a file whose blocks are in more than one allocation group.
//!
//! Every other write fixture keeps its files inside one group, because
//! that is what an allocator does when there is room. A file bigger than
//! a group has no choice, and then nothing about "the group" is singular
//! any more: the free space goes back to two different headers, two sets
//! of free-space trees and two reverse maps.
//!
//! `truncate_to_zero` refused it:
//!
//! ```text
//! inode N has extents in allocation groups 1 and 3; freeing across
//! groups is not implemented
//! ```
//!
//! Safe, and a file larger than a group is ordinary rather than exotic —
//! 75 MB groups and a 100 MB file is all it takes.

mod common;
use common::{kernel_run, share};

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let path = share().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Which groups a file's extents are in.
fn groups_of(fs: &Filesystem, path: &str) -> Vec<u32> {
    let inode = fs.lookup_path(path).expect("the file");
    let (inode, raw) = fs.read_inode_raw(inode.ino).expect("read it");
    let mut groups: Vec<u32> = fs
        .data_extents(&inode, &raw)
        .expect("its extents")
        .iter()
        .map(|e| fs.superblock().split_fsblock(e.startblock).0)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

/// Free the spanning file and let the kernel and the checker judge.
fn case(name: &str) -> bool {
    let source = share().join(format!("xfscrossag-{name}.img"));
    if !source.exists() {
        eprintln!("no xfscrossag-{name} fixture — skipping");
        return false;
    }
    let scratch_name = format!("xfs-crossag-{name}-scratch.img");
    let scratch = Scratch::from(&source, &scratch_name);

    let (ino, groups, free_before) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(scratch.path()).expect("open")))
            .expect("mount");
        let groups = groups_of(&fs, "/spanning");
        assert!(
            groups.len() > 1,
            "{name}: the fixture's file must span groups, or this proves nothing; it is in {groups:?}"
        );
        let free: Vec<u32> = (0..fs.superblock().agcount)
            .map(|ag| {
                fs.free_extents(ag)
                    .map(|e| e.iter().map(|x| x.blockcount).sum())
                    .unwrap_or(0)
            })
            .collect();
        (
            fs.lookup_path("/spanning").expect("the file").ino,
            groups,
            free,
        )
    };

    {
        let dev = FileDevice::open_rw(scratch.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        fs.truncate_to_zero(ino)
            .unwrap_or_else(|e| panic!("{name}: freeing across groups should work: {e}"));
    }

    let script = format!(
        r#"
        m=$(mktemp -d)
        mounted=0
        for attempt in 1 2 3; do
            if mount -o loop,nouuid /share/{scratch_name} "$m"; then
                [ "$(stat -c%s "$m/spanning")" = 0 ] && echo "EMPTIED" || echo "NOT_EMPTIED"
                # The file that was inside one group has to be untouched.
                [ "$(stat -c%s "$m/withingroup")" = 262144 ] && echo "NEIGHBOUR_OK" || echo "NEIGHBOUR_CHANGED"
                umount "$m"
                mounted=$((mounted + 1))
            fi
            check=$(mktemp -u /tmp/repair-XXXXXX.img)
            cp /share/{scratch_name} "$check"
            out=$(xfs_repair -n "$check" 2>&1) && rc=0 || rc=$?
            rm -f "$check"
            case "$out" in
                *"valuable metadata changes in a log"*) continue ;;
                *) break ;;
            esac
        done
        rmdir "$m" 2>/dev/null
        [ "$mounted" -gt 0 ] || echo "MOUNT_FAILED"
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
        echo "REPAIR_END"
        echo DONE
        "#
    );

    let Some(out) = kernel_run(&script) else {
        eprintln!("no kernel to replay the record — skipping the check");
        return false;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{name}: the kernel refused the filesystem after a cross-group free:\n{out}"
    );
    assert!(
        out.contains("EMPTIED"),
        "{name}: the file still holds bytes after being truncated\n{out}"
    );
    assert!(
        out.contains("NEIGHBOUR_OK"),
        "{name}: freeing one file changed another\n{out}"
    );
    assert!(
        out.contains("REPAIR_RC=0"),
        "{name}: xfs_repair objected after a cross-group free:\n{out}"
    );

    // EVERY GROUP THE FILE WAS IN GOT ITS BLOCKS BACK. Checking the
    // total alone would pass if one group received all of them, which is
    // the mistake this operation is most likely to make.
    let fs = Filesystem::mount(Arc::new(
        FileDevice::open(scratch.path()).expect("open after the replay"),
    ))
    .expect("mount after the replay");
    for ag in &groups {
        let after: u32 = fs
            .free_extents(*ag)
            .expect("free space")
            .iter()
            .map(|e| e.blockcount)
            .sum();
        let before = free_before[*ag as usize];
        assert!(
            after > before,
            "{name}: group {ag} held part of the file and got nothing back \
             ({before} blocks free before, {after} after)"
        );
    }
    eprintln!("{name}: freed inode {ino} across groups {groups:?}");
    true
}

/// A file across several groups is freed, and every group involved gets
/// its blocks back.
#[test]
fn a_file_across_groups_is_freed_into_all_of_them() {
    let mut ran = Vec::new();
    // With and without a reverse map: the map is the harder one, because
    // each group keeps its own and a record has to come out of each.
    for name in ["plain", "rmap"] {
        if case(name) {
            ran.push(name);
        }
    }
    if ran.is_empty() {
        eprintln!(
            "no xfscrossag fixtures — skipping. Build them with \
             `sudo ./scripts/build-crossag-fixtures.sh` (needs xfsprogs, so Linux)."
        );
        return;
    }
    eprintln!("freed across groups for: {ran:?}");
}
