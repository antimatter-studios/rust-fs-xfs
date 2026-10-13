//! What the journalled C ABI writes is a volume the reference tools and
//! the kernel agree with (#460).
//!
//! The guest's own `mkfs.xfs` makes the volume. The driver then mounts it
//! for writing through `fs_xfs_mount_rw_with_fs_core_device` and changes
//! it through nothing but the C ABI — every namespace call, allocating
//! writes, truncate both ways, attributes and the label after a logged
//! operation, an extended attribute and a POSIX ACL — and releases the
//! handle with `fs_xfs_umount`. With no replay first, `xfs_repair -n`
//! must find nothing, `xfs_logprint` must call the log clean, and the
//! kernel must mount it without recovery and read back what the calls
//! said.
//!
//! `tests/capi_write_ops.rs` pins the ABI's return values and errnos;
//! this pins what lands on the disk.

mod common;

use common::{guest_quote, kernel_run, oracle, repair, scratch};
use fs_core::ffi::FsCoreDevice;
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::capi::*;
use std::ffi::CString;
use std::sync::Arc;

const SUITE: &str = "capi_write_ops_oracle";

fn c(s: &str) -> CString {
    CString::new(s).unwrap()
}

#[track_caller]
fn ok(result: i64, what: &str) {
    assert!(
        result >= 0,
        "{what} failed: errno {}, {}",
        fs_xfs_last_errno(),
        unsafe { std::ffi::CStr::from_ptr(fs_xfs_last_error()) }.to_string_lossy()
    );
}

#[track_caller]
fn made(ino: u64, what: &str) {
    assert_ne!(ino, 0, "{what} failed: errno {}", fs_xfs_last_errno());
}

/// The bytes of the main file: a pattern, a hole, a tail.
fn expected_file() -> Vec<u8> {
    let mut bytes: Vec<u8> = (0..70_004u32).map(|i| (i % 251) as u8).collect();
    bytes[5000..70_000].fill(0);
    bytes[70_000..].copy_from_slice(b"tail");
    bytes[10..13].copy_from_slice(b"XYZ");
    bytes
}

#[test]
fn a_volume_changed_through_the_c_abi_is_clean_and_reads_back_in_the_kernel() {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-capi.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());
    let formatted = kernel_run(&format!("mkfs.xfs -q -f {image}\necho DONE\n"));
    assert!(formatted.contains("DONE"), "{formatted}");

    let expected = scratch::dir(SUITE).join(format!("{}-expected", std::process::id()));
    std::fs::write(&expected, expected_file()).expect("write the expected bytes");
    let expected_guest = guest_quote(&scratch::guest_path(&expected));

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let handle = FsCoreDevice::into_handle(Arc::new(dev) as Arc<dyn BlockDevice>);
        let fs = unsafe { fs_xfs_mount_rw_with_fs_core_device(handle) };
        unsafe { fs_core::ffi::fs_core_device_close(handle) };
        assert!(!fs.is_null(), "mount: errno {}", fs_xfs_last_errno());

        unsafe {
            made(fs_xfs_mkdir(fs, c("/dir").as_ptr(), 0o755), "mkdir /dir");
            // Logged, since the mkdir was (#464).
            ok(
                fs_xfs_set_label(fs, c("CAPILABEL").as_ptr()).into(),
                "set the label after a logged change",
            );
            made(fs_xfs_create(fs, c("/dir/file").as_ptr(), 0o644), "create");
            let pattern: Vec<u8> = (0..5000u32).map(|i| (i % 251) as u8).collect();
            let file = c("/dir/file");
            ok(
                fs_xfs_write_file(fs, file.as_ptr(), pattern.as_ptr().cast(), 0, 5000),
                "write into the empty file",
            );
            ok(
                fs_xfs_write_file(fs, file.as_ptr(), b"tail".as_ptr().cast(), 70_000, 4),
                "write past the end",
            );
            ok(
                fs_xfs_write_file(fs, file.as_ptr(), b"XYZ".as_ptr().cast(), 10, 3),
                "overwrite after logged changes",
            );
            ok(
                fs_xfs_set_attributes(
                    fs,
                    file.as_ptr(),
                    0o600,
                    1234,
                    2345,
                    1_600_000_000,
                    123_456_789,
                    1_700_000_000,
                    987_654_321,
                )
                .into(),
                "set attributes",
            );
            let (name, value) = (c("user.color"), b"blue");
            ok(
                fs_xfs_setxattr(
                    fs,
                    file.as_ptr(),
                    name.as_ptr(),
                    value.as_ptr().cast(),
                    4,
                    0,
                )
                .into(),
                "setxattr",
            );
            ok(
                fs_xfs_link(fs, file.as_ptr(), c("/dir/hard").as_ptr()).into(),
                "link",
            );

            made(
                fs_xfs_create(fs, c("/dir/victim").as_ptr(), 0o644),
                "victim",
            );
            ok(
                fs_xfs_write_file(fs, c("/dir/victim").as_ptr(), b"old".as_ptr().cast(), 0, 3),
                "write victim",
            );
            made(
                fs_xfs_create(fs, c("/dir/moving").as_ptr(), 0o644),
                "moving",
            );
            ok(
                fs_xfs_write_file(fs, c("/dir/moving").as_ptr(), b"new".as_ptr().cast(), 0, 3),
                "write moving",
            );
            ok(
                fs_xfs_rename2(
                    fs,
                    c("/dir/moving").as_ptr(),
                    c("/dir/victim").as_ptr(),
                    FS_XFS_RENAME_REPLACE,
                )
                .into(),
                "rename over a name",
            );

            made(fs_xfs_mkdir(fs, c("/dir/sub").as_ptr(), 0o700), "mkdir sub");
            ok(
                fs_xfs_rename(fs, c("/dir/sub").as_ptr(), c("/moved").as_ptr()).into(),
                "move a directory across",
            );
            made(fs_xfs_mkdir(fs, c("/gone").as_ptr(), 0o755), "mkdir gone");
            ok(fs_xfs_rmdir(fs, c("/gone").as_ptr()).into(), "rmdir");
            made(
                fs_xfs_create(fs, c("/dir/unl").as_ptr(), 0o644),
                "create unl",
            );
            ok(fs_xfs_unlink(fs, c("/dir/unl").as_ptr()).into(), "unlink");

            made(fs_xfs_create(fs, c("/grow").as_ptr(), 0o644), "create grow");
            ok(
                fs_xfs_truncate(fs, c("/grow").as_ptr(), 12_345, FS_XFS_LEAVE_TIME, 0).into(),
                "truncate up",
            );
            made(
                fs_xfs_create(fs, c("/shrink").as_ptr(), 0o644),
                "create shrink",
            );
            let big = vec![b's'; 40_000];
            ok(
                fs_xfs_write_file(fs, c("/shrink").as_ptr(), big.as_ptr().cast(), 0, 40_000),
                "write shrink",
            );
            ok(
                fs_xfs_truncate(fs, c("/shrink").as_ptr(), 100, FS_XFS_LEAVE_TIME, 0).into(),
                "truncate down",
            );

            made(fs_xfs_create(fs, c("/acl").as_ptr(), 0o644), "create acl");
            let mut acl = 2u32.to_le_bytes().to_vec();
            for (tag, perm, id) in [
                (0x01u16, 6u16, u32::MAX),
                (0x02, 4, 1000),
                (0x04, 4, u32::MAX),
                (0x10, 4, u32::MAX),
                (0x20, 0, u32::MAX),
            ] {
                acl.extend_from_slice(&tag.to_le_bytes());
                acl.extend_from_slice(&perm.to_le_bytes());
                acl.extend_from_slice(&id.to_le_bytes());
            }
            ok(
                fs_xfs_setxattr(
                    fs,
                    c("/acl").as_ptr(),
                    c("system.posix_acl_access").as_ptr(),
                    acl.as_ptr().cast(),
                    acl.len(),
                    0,
                )
                .into(),
                "set an access ACL",
            );

            ok(fs_xfs_flush(fs).into(), "flush");
            fs_xfs_umount(fs);
        }
        assert_eq!(fs_xfs_last_errno(), 0, "the unmount failed");
    }

    // No mount first: the tools read the volume as the driver left it.
    let path = volume.path().to_str().unwrap();
    let report = oracle("xfs_repair").args(["-n", path]).output();
    repair::assert_agreed(
        &report.repair_report(),
        "a volume changed through the C ABI and released with fs_xfs_umount",
    );
    let label = oracle("xfs_db").args(["-r", "-c", "label", path]).output();
    assert!(
        label.stdout.contains("label = \"CAPILABEL\""),
        "xfs_db does not read the label set through the C ABI:\n{}",
        label.stdout
    );
    let log = oracle("xfs_logprint").args(["-t", path]).output();
    assert!(
        log.stdout.contains("state: <CLEAN>"),
        "xfs_logprint does not call the log clean:\n{}",
        log.stdout
    );

    let out = kernel_run(&format!(
        r#"
        dmesg -C
        m=$(mktemp -d)
        mount -o loop,noatime {image} "$m" && echo MOUNT_OK
        cmp "$m/dir/file" {expected_guest} && echo CONTENT_OK
        echo "FILE $(stat -c '%a %u %g %h %s %X %Y' "$m/dir/file")"
        echo "ATIME $(TZ=UTC stat -c '%x' "$m/dir/file")"
        echo "MTIME $(TZ=UTC stat -c '%y' "$m/dir/file")"
        echo "SAME $(stat -c %i "$m/dir/file") $(stat -c %i "$m/dir/hard")"
        echo "COLOR $(getfattr --only-values -n user.color "$m/dir/file")"
        echo "VICTIM $(cat "$m/dir/victim")"
        [ -e "$m/dir/moving" ] || echo MOVING_GONE
        [ -e "$m/dir/unl" ] || echo UNL_GONE
        [ -e "$m/gone" ] || echo GONE_GONE
        [ -e "$m/dir/sub" ] || echo SUB_GONE
        echo "MOVED $(stat -c '%F %a %h' "$m/moved")"
        echo "DIR $(stat -c '%h' "$m/dir")"
        echo "ROOT $(stat -c '%h' "$m")"
        echo "GROW $(stat -c %s "$m/grow") $(tr -d '\000' < "$m/grow" | wc -c)"
        echo "SHRINK $(stat -c %s "$m/shrink") $(tr -d 's' < "$m/shrink" | wc -c)"
        echo "ACLMODE $(stat -c %a "$m/acl")"
        getfacl -n -c "$m/acl" | sed 's/^/FACL /'
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        dmesg | grep -i "recovery" || echo NO_RECOVERY
        echo DONE
        "#
    ));
    for want in [
        "MOUNT_OK",
        "CONTENT_OK",
        "FILE 600 1234 2345 2 70004 1600000000 1700000000",
        "ATIME 2020-09-13 12:26:40.123456789 +0000",
        "MTIME 2023-11-14 22:13:20.987654321 +0000",
        "COLOR blue",
        "VICTIM new",
        "MOVING_GONE",
        "UNL_GONE",
        "GONE_GONE",
        "SUB_GONE",
        "MOVED directory 700 2",
        "DIR 2",
        "ROOT 4",
        "GROW 12345 0",
        "SHRINK 100 0",
        "ACLMODE 640",
        "FACL user::rw-",
        "FACL user:1000:r--",
        "FACL group::r--",
        "FACL mask::r--",
        "FACL other::---",
        "NO_RECOVERY",
        "DONE",
    ] {
        assert!(
            out.contains(want),
            "the kernel did not report {want:?}:\n{out}"
        );
    }
    let same = out
        .lines()
        .find_map(|l| l.strip_prefix("SAME "))
        .expect("SAME line");
    let mut inodes = same.split_whitespace();
    assert_eq!(
        inodes.next(),
        inodes.next(),
        "the hard link names another inode:\n{out}"
    );
    std::fs::remove_file(&expected).ok();
}
