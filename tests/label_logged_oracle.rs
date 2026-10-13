//! A label set after a logged change is logged too, and the kernel's
//! replay of that record is what puts it on the volume (#464).
//!
//! The label used to be written in place only, so a mount that had
//! logged anything refused it: a replay of the earlier records would
//! write their superblock image over the new name. Logged, it is one
//! more record, and the only party that can say the record is right is
//! the one that replays it.
//!
//! # The shape of the proof
//!
//! The guest's own `mkfs.xfs` makes the volume. The driver logs a
//! create, the label and a second create, and stops without unmounting,
//! so the primary superblock on disk still holds the old label and only
//! the log holds the new one. Then:
//!
//! - the kernel mounts it, replaying the log, and reports the new label
//!   through `FS_IOC_GETFSLABEL` — only a replayed record can have put it
//!   there;
//! - both files are present, so the records either side of the label's
//!   were replayed with it;
//! - after the kernel unmounts, `xfs_db` reads the label from the primary
//!   superblock and from a secondary, which the driver writes in place,
//!   and `xfs_repair -n` finds nothing wrong.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::{guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "label_logged_oracle";

#[test]
fn a_label_logged_after_a_create_is_replayed_by_the_kernel() {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-label.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let formatted = kernel_run(&format!("mkfs.xfs -q -f -L BEFORE {image}\necho DONE\n"));
    assert!(formatted.contains("DONE"), "{formatted}");

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        fs.create_file(root, b"before", 0o644)
            .expect("the create before the label");
        fs.set_label("LOGGED")
            .expect("a label after a logged change is logged too");
        fs.create_file(root, b"after", 0o644)
            .expect("the create after the label");
        // Dropped without unmounting: the records are all the volume
        // has of these changes, and the kernel has to replay them.
    }

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        echo "DISK_LABEL $(xfs_db -r -c label "$img")"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "MOUNTED_LABEL $(xfs_io -c label "$m")"
            echo "NAMES $(ls -A "$m" | sort | tr '\n' ' ')"
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "PRIMARY_LABEL $(xfs_db -r -c label "$img")"
        echo "SECONDARY_LABEL $(xfs_db -r -c 'sb 1' -c label "$img")"
        {repair}
        rm -f "$img"
        echo "DONE"
        "#,
        repair = repair::script("\"$img\""),
    );
    let out = kernel_run(&script);

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume after the label was logged:\n{out}"
    );
    assert!(
        !out.contains("UMOUNT_FAILED"),
        "the kernel could not unmount the volume:\n{out}"
    );
    let line = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(key))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(
        line("DISK_LABEL"),
        "label = \"BEFORE\"",
        "before the replay the primary superblock should still hold the old label, so \
         that only the log can carry the new one"
    );
    assert_eq!(
        line("MOUNTED_LABEL"),
        "label = \"LOGGED\"",
        "the kernel's replay did not put the logged label on the volume"
    );
    assert_eq!(
        line("NAMES"),
        "after before",
        "the creates either side of the label were not both replayed"
    );
    assert_eq!(line("PRIMARY_LABEL"), "label = \"LOGGED\"");
    assert_eq!(line("SECONDARY_LABEL"), "label = \"LOGGED\"");
    repair::assert_agreed(&out, "a volume whose label was logged after a create");
}
