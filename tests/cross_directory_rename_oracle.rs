//! Names this driver moves between directories are where the Linux kernel
//! finds them, and a moved directory's `..` is its new parent (#382).
//!
//! The volume is made by `mkfs.xfs` in the harness guest. On one mount the
//! driver makes `a`, `b` and `big` (300 names, so leaf form), and then:
//! moves the file `a/f` to `b/g`; moves the directory `a/sub`, which holds
//! a file, to `b/sub`; moves a name out of `big` into `a`; and moves a name
//! from `b` into `big`.
//!
//! The kernel mounts the result, which replays the records, and must list
//! exactly the names each directory should hold, report `b/sub/..` as `b`,
//! and give `a` and `b` the link counts the move of `sub` leaves them;
//! `xfs_repair -n` must call the volume clean.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::collections::BTreeSet;
use std::sync::Arc;

const SUITE: &str = "cross_directory_rename_oracle";

fn big_name(i: usize) -> String {
    format!("name-in-a-big-directory-{i:04}")
}

#[test]
fn the_kernel_finds_names_moved_between_directories() {
    let volume = scratch::Volume::empty(SUITE, "moves.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        "mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK; echo DONE"
    ));
    assert!(built.contains("MKFS_OK"), "mkfs.xfs failed:\n{built}");

    let b_ino;
    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        let (a, _) = fs.create_directory(root, b"a", 0o040755).expect("mkdir a");
        let (b, _) = fs.create_directory(root, b"b", 0o040755).expect("mkdir b");
        let (big, _) = fs
            .create_directory(root, b"big", 0o040755)
            .expect("mkdir big");
        b_ino = b;
        fs.create_file(a, b"f", 0o100644).expect("create f");
        let (sub, _) = fs.create_directory(a, b"sub", 0o040755).expect("mkdir sub");
        fs.create_file(sub, b"inside", 0o100644)
            .expect("create inside");
        fs.create_file(b, b"from-b", 0o100644)
            .expect("create from-b");
        for i in 0..300 {
            fs.create_file(big, big_name(i).as_bytes(), 0o100644)
                .expect("create in big");
        }
        fs.rename(a, b"f", b, b"g").expect("move a file");
        fs.rename(a, b"sub", b, b"sub").expect("move a directory");
        fs.rename(big, big_name(7).as_bytes(), a, b"out-of-big")
            .expect("move out of leaf form");
        fs.rename(b, b"from-b", big, b"into-big")
            .expect("move into leaf form");
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            for d in a b b/sub big; do
                ls -1 "$m/$d" | sed "s|^|NAME $d |"
            done
            echo "DOTDOT $(stat -c %i "$m/b/sub/..")"
            echo "LINKS a $(stat -c %h "$m/a")"
            echo "LINKS b $(stat -c %h "$m/b")"
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
    repair::assert_agreed(&out, "the volume after moves between directories");
    let listed = |dir: &str| -> BTreeSet<String> {
        out.lines()
            .filter_map(|l| l.trim().strip_prefix(&format!("NAME {dir} ")))
            .map(str::to_string)
            .collect()
    };
    let set = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
    assert_eq!(listed("a"), set(&["out-of-big"]), "a:\n{out}");
    assert_eq!(listed("b"), set(&["g", "sub"]), "b:\n{out}");
    assert_eq!(listed("b/sub"), set(&["inside"]), "b/sub:\n{out}");
    let mut big: BTreeSet<String> = (0..300).filter(|&i| i != 7).map(big_name).collect();
    big.insert("into-big".into());
    assert_eq!(listed("big"), big, "big");
    assert!(
        out.lines().any(|l| l.trim() == format!("DOTDOT {b_ino}")),
        "b/sub/.. is not b ({b_ino}):\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim() == "LINKS a 2"),
        "a's links:\n{out}"
    );
    assert!(
        out.lines().any(|l| l.trim() == "LINKS b 3"),
        "b's links:\n{out}"
    );
}
