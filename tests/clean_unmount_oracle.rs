//! A volume this driver wrote and unmounted is one `xfs_repair -n` grades
//! with no replay first, whose log `xfs_logprint` calls clean, and which
//! the kernel mounts without recovery (#439).
//!
//! Before an unmount existed, every oracle had to have the kernel mount a
//! driver-written volume, replaying its log, before `xfs_repair` would
//! look at it; a volume with a feature the guest's kernel cannot mount
//! could not be graded at all. Here the guest's own `mkfs.xfs` makes the
//! volume, the driver writes files and a directory into it and unmounts,
//! and the tools read it as it lies.

mod common;

use common::{guest_quote, kernel_run, oracle, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

const SUITE: &str = "clean_unmount_oracle";

#[test]
fn an_unmounted_volume_needs_no_replay_to_be_graded_or_mounted() {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-unmounted.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let made = kernel_run(&format!("mkfs.xfs -q -f {image}\necho DONE\n"));
    assert!(made.contains("DONE"), "{made}");

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.superblock().rootino;
        let (dir, _) = fs.create_directory(root, b"d", 0o040755).expect("mkdir");
        for i in 0..30 {
            let (ino, _) = fs
                .create_file(dir, format!("f{i:02}").as_bytes(), 0o100644)
                .expect("create");
            fs.write(ino, 0, &vec![b'a' + (i % 26) as u8; 5000 + i * 1000])
                .expect("write");
        }
        fs.unmount().expect("unmount");
    }

    // No mount first: the tools read the volume as the driver left it.
    let path = volume.path().to_str().unwrap();
    let report = oracle("xfs_repair").args(["-n", path]).output();
    repair::assert_agreed(
        &report.repair_report(),
        "a driver-written volume after its unmount, with no replay first",
    );
    let log = oracle("xfs_logprint").args(["-t", path]).output();
    // `-t` reads the log's own state: CLEAN only when the head is an
    // unmount record and the tail has caught up with it.
    assert!(
        log.stdout.contains("state: <CLEAN>"),
        "xfs_logprint does not call the log clean:\n{}",
        log.stdout
    );

    let mounted = kernel_run(&format!(
        r#"
        dmesg -C
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        echo "SIZE $(stat -c %s "$m/d/f29")"
        echo "COUNT $(ls -1 "$m/d" | wc -l)"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        dmesg | grep -i "recovery" || echo NO_RECOVERY
        echo DONE
        "#
    ));
    assert!(
        mounted.contains("MOUNT_OK"),
        "the kernel refused it:\n{mounted}"
    );
    assert!(
        mounted.contains("NO_RECOVERY"),
        "the kernel recovered a log the unmount said was clean:\n{mounted}"
    );
    assert!(mounted.contains("SIZE 34000"), "{mounted}");
    assert!(mounted.contains("COUNT 30"), "{mounted}");
}
