//! Several journalled operations in one mount replay to a volume the
//! kernel and `xfs_repair` accept (#89).
//!
//! A journalled operation writes a record and touches nothing on disk, so
//! a second operation in the same mount used to read a disk that did not
//! reflect the first: two creates would hand out one inode number. The
//! mount therefore refused the second outright.
//!
//! What makes more than one safe is that every operation's own record is
//! applied to an overlay the mount reads through, so each operation is
//! built on the state the one before it logged. The kernel is the judge:
//! it replays the records in order, and the volume it arrives at must hold
//! exactly what the sequence describes.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "many_ops_per_mount";
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Removes the image however the test ends, and the share if this made it:
/// every suite reads each `.img` there as a fixture.
#[test]
fn a_mount_writes_several_journalled_operations() {
    // NO FIXTURE DIRECTORY MEANS NO FIXTURE SET. This builds its own
    // volume, but it builds it in the share, and a share that exists is
    // what the suites scanning it take for a fixture set: creating one
    // here makes them fail where they would have skipped. The job that
    // runs this builds the fixtures first, so the directory is there.
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        320 * 1024 * 1024,
    );
    let image = scratch.path().to_path_buf();
    let name = scratch.guest();
    let Some(mkfs) = kernel_run(&format!(
        "mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK; echo DONE"
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(mkfs.contains("MKFS_OK"), "mkfs.xfs failed:\n{mkfs}");
    let path = image.to_str().unwrap().to_string();

    // One mount, eight journalled operations, each built on the last.
    {
        let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.lookup_path("/").unwrap().ino;
        fs.create_file(root, b"first", 0o100644)
            .expect("create /first");
        fs.create_file(root, b"second", 0o100644)
            .expect("create /second: the second operation must not reuse the first's inode");
        let (dir, _) = fs.create_directory(root, b"d", 0o40755).expect("mkdir /d");
        fs.create_file(dir, b"inside", 0o100644)
            .expect("create /d/inside: a create inside a directory this mount made");
        fs.unlink_file(root, b"first").expect("unlink /first");
        // Every remaining journalled operation, each reading what the ones
        // before it logged: a rename in a directory this mount made, a
        // first write into a file it made, and a truncate of that file.
        fs.rename_in_directory(dir, b"inside", b"renamed")
            .expect("rename /d/inside");
        let written = fs.lookup_path("/d/renamed").expect("the renamed file").ino;
        fs.write_into_empty_file(written, &[0xD7; 8192])
            .expect("write into /d/renamed");
        let grown = fs.lookup_path("/d/renamed").expect("after the write");
        assert_eq!(
            grown.size, 8192,
            "the mount's own view of the file it wrote"
        );
        fs.truncate_to_zero(written).expect("truncate /d/renamed");
    }

    // The kernel replays every record, then says what the volume holds.
    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {name} "$m"; then
            echo "ROOT $(ls "$m" | sort | tr '\n' ' ')"
            echo "DIR $(ls "$m/d" 2>&1 | sort | tr '\n' ' ')"
            echo "SIZE $(stat -c %s "$m/d/renamed")"
            echo "INOS $(stat -c %i "$m/second" "$m/d" "$m/d/renamed" | sort -u | wc -l)"
            umount "$m"
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ))
    .expect("kernel");

    assert!(
        out.contains("MOUNTED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, "the volume after the run");
    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(field("ROOT"), "d second", "the root after every operation");
    assert_eq!(
        field("DIR"),
        "renamed",
        "what the directory this mount made holds, after the rename"
    );
    assert_eq!(
        field("SIZE"),
        "0",
        "the truncate of a file this mount wrote in the same mount"
    );
    assert_eq!(
        field("INOS"),
        "3",
        "every inode this mount created must be a different one"
    );
}
