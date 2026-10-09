//! Directories this driver moves into leaf form and back are the ones
//! the Linux kernel lists and `xfs_repair` accepts (#366).
//!
//! The volume is made by `mkfs.xfs` in the harness guest. On one mount the
//! driver makes `big` and fills it with 300 files, which takes it from
//! short form through block form into leaf form; makes `shrunk` the same
//! way and then removes all but ten of its names, which packs it back into
//! one block; and makes `kept` in leaf form and removes a third of it,
//! which leaves it in leaf form with fewer data blocks. One name in each is
//! then renamed to a longer one.
//!
//! The kernel mounts the result, which replays the records, and must list
//! exactly the names each directory should hold, and `xfs_db` must report
//! each directory in the format its size calls for. `xfs_repair -n` must
//! call the volume clean, which is the check on every leaf's hash index,
//! every data block's free space and every block given back.

mod common;

use common::{kernel_run, oracle, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::collections::BTreeSet;
use std::sync::Arc;

const SUITE: &str = "leaf_directories_oracle";

fn name(i: usize) -> String {
    format!("a-file-in-a-large-directory-{i:04}")
}

#[test]
fn the_kernel_lists_directories_moved_into_leaf_form_and_back() {
    let volume = scratch::Volume::empty(SUITE, "leaf.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        echo DONE
        "#
    ));
    assert!(built.contains("MKFS_OK"), "mkfs.xfs failed:\n{built}");

    let mut want: Vec<(&str, BTreeSet<String>)> = Vec::new();
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        for (dir_name, keep) in [("big", 300), ("shrunk", 10), ("kept", 200)] {
            let (dir, _) = fs
                .create_directory(root, dir_name.as_bytes(), 0o040755)
                .expect("mkdir");
            for i in 0..300 {
                fs.create_file(dir, name(i).as_bytes(), 0o100644)
                    .unwrap_or_else(|e| panic!("{dir_name}: create {i}: {e:?}"));
            }
            for i in keep..300 {
                fs.unlink_file(dir, name(i).as_bytes())
                    .unwrap_or_else(|e| panic!("{dir_name}: unlink {i}: {e:?}"));
            }
            let mut names: BTreeSet<String> = (0..keep).map(name).collect();
            // And one rename, to a longer name, in each.
            let renamed = format!("renamed-in-{dir_name}-to-a-considerably-longer-name");
            fs.rename_in_directory(dir, name(3).as_bytes(), renamed.as_bytes())
                .unwrap_or_else(|e| panic!("{dir_name}: rename: {e:?}"));
            names.remove(&name(3));
            names.insert(renamed);
            want.push((dir_name, names));
        }
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            for d in big shrunk kept; do
                ls -1 "$m/$d" | sed "s|^|NAME $d |"
            done
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
        "#
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(
        &out,
        "the volume after directories moved into leaf form and back",
    );
    for (dir, names) in &want {
        let listed: BTreeSet<String> = out
            .lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("NAME {dir} ")))
            .map(str::to_string)
            .collect();
        assert_eq!(&listed, names, "{dir}: the kernel lists other names");
    }

    // The shape each directory should have: leaf form while it needs more
    // than one block, block form once it fits one again.
    for (dir, leaf) in [("big", true), ("shrunk", false), ("kept", true)] {
        let ino = {
            let dev = FileDevice::open(volume.path().to_str().unwrap()).expect("open");
            let fs =
                Filesystem::mount(Arc::new(dev) as Arc<dyn fs_core::BlockRead>).expect("mount");
            fs.lookup_path(&format!("/{dir}")).expect(dir).ino
        };
        let shown = oracle("xfs_db")
            .args([
                "-r",
                "-c",
                &format!("inode {ino}"),
                "-c",
                "p core.nextents core.size",
            ])
            .arg(volume.path())
            .output();
        let nextents: u64 = shown
            .stdout
            .lines()
            .find_map(|l| l.trim().strip_prefix("core.nextents = "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or_else(|| panic!("{dir}: xfs_db printed no extent count:\n{}", shown.stdout));
        if leaf {
            assert!(
                nextents >= 2,
                "{dir}: {nextents} extent(s), so no leaf block"
            );
        } else {
            assert_eq!(
                nextents, 1,
                "{dir}: shrunk to ten names but not back to one block"
            );
        }
    }
}
