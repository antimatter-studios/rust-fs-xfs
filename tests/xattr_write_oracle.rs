//! Attribute writes must survive kernel recovery and independent repair.
mod common;

use common::{kernel_run, oracle, scratch};
use fs_core::FileDevice;
use fs_xfs::{attr_write::XattrMode, Filesystem};
use std::sync::Arc;

fn fresh(name: &str) -> scratch::Volume {
    let volume = scratch::Volume::empty("xattr_write_oracle", name, 300 << 20);
    let made = oracle("mkfs.xfs")
        .args(["-q", "-f", "-m", "rmapbt=1"])
        .arg(volume.path())
        .output();
    assert!(made.ok(), "{}{}", made.stdout, made.stderr);
    volume
}

fn check_kernel(volume: &scratch::Volume, attrs: &[fs_xfs::attr::Xattr], edit: &str) {
    let expected = volume.path().with_extension("json");
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let entries: Vec<_> = attrs
        .iter()
        .map(|a| format!("[\"{}\",\"{}\"]", hex(&a.name), hex(&a.value)))
        .collect();
    std::fs::write(&expected, format!("[{}]", entries.join(","))).unwrap();
    kernel_run(&format!(
        r#"
        set -eu
        img=$(mktemp /var/tmp/xattr-389-XXXXXX.img)
        cp --sparse=always {source} "$img"
        m=$(mktemp -d)
        mount -o loop,nouuid "$img" "$m"
        python3 - "$m" {expected} <<'PY'
import os, sys, json
path = sys.argv[1]
expected = {{bytes.fromhex(n): bytes.fromhex(v) for n, v in json.load(open(sys.argv[2]))}}
actual = {{os.fsencode(n): os.getxattr(path, n) for n in os.listxattr(path)}}
assert actual == expected, [(n, len(v)) for n, v in actual.items()]
{edit}
PY
        umount "$m" || {{ sleep 2; umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}; }}
        cp --sparse=always "$img" {source}
        rm "$img"
        rmdir "$m"
        echo DONE
    "#,
        source = volume.guest(),
        expected = common::guest_quote(expected.to_str().unwrap())
    ));
    common::assert_xfs_repair_clean(volume.path().to_str().unwrap(), "attribute transition");
    let db = oracle("xfs_db")
        .args(["-r", "-c", "sb 0", "-c", "print magicnum"])
        .arg(volume.path())
        .output();
    assert!(
        db.ok() && db.stdout.contains("0x58465342"),
        "{}{}",
        db.stdout,
        db.stderr
    );
}

#[test]
fn every_value_boundary_and_namespace_round_trips() {
    for size in [0, 1, 255, 256, 2017, 2018, 4040, 4041, 65536] {
        let volume = fresh(&format!("boundary-{size}.img"));
        let attrs;
        {
            let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap()))
                .unwrap();
            let ino = fs.superblock().rootino;
            let value: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            for name in [
                b"user.boundary".as_slice(),
                b"trusted.boundary",
                b"security.boundary",
            ] {
                fs.set_xattr(ino, name, &value, XattrMode::Create).unwrap();
                assert_eq!(
                    fs.set_xattr(ino, name, b"wrong", XattrMode::Create),
                    Err(fs_xfs::Error::AlreadyExists)
                );
                let (inode, raw) = fs.read_inode_raw(ino).unwrap();
                assert_eq!(
                    fs.get_xattr(&inode, &raw, name).unwrap(),
                    Some(value.clone())
                );
            }
            let (inode, raw) = fs.read_inode_raw(ino).unwrap();
            attrs = fs.list_xattrs(&inode, &raw).unwrap();
        }
        check_kernel(
            &volume,
            &attrs,
            "os.setxattr(path, b'user.kernel', b'kernel'); os.removexattr(path, b'user.kernel')",
        );
    }
}

#[test]
fn fork_grows_to_nodes_and_shrinks_to_empty() {
    let volume = fresh("transitions.img");
    let mutate = |action: usize| {
        let fs =
            Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap())).unwrap();
        let ino = fs.superblock().rootino;
        match action {
            0 => {
                fs.set_xattr(ino, b"user.keep", b"inline", XattrMode::Set)
                    .unwrap();
            }
            1 => {
                fs.set_xattr(ino, b"user.keep", &[8; 256], XattrMode::Replace)
                    .unwrap();
            }
            2 => {
                for i in 0..80 {
                    fs.set_xattr(
                        ino,
                        format!("user.entry{i:03}").as_bytes(),
                        &[i as u8; 200],
                        XattrMode::Create,
                    )
                    .unwrap();
                }
            }
            3 => {
                fs.set_xattr(ino, b"user.keep", &[9; 65536], XattrMode::Replace)
                    .unwrap();
            }
            4 => {
                for i in 0..80 {
                    fs.remove_xattr(ino, format!("user.entry{i:03}").as_bytes())
                        .unwrap();
                }
            }
            5 => {
                fs.set_xattr(ino, b"user.keep", b"inline again", XattrMode::Replace)
                    .unwrap();
            }
            6 => {
                fs.remove_xattr(ino, b"user.keep").unwrap();
            }
            _ => unreachable!(),
        }
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        let attrs = fs.list_xattrs(&inode, &raw).unwrap();
        if matches!(action, 0 | 5) {
            assert_eq!(inode.aformat, fs_xfs::inode::Format::Local);
        }
        if matches!(action, 1..=4) {
            assert_eq!(inode.aformat, fs_xfs::inode::Format::Extents);
        }
        if action == 6 {
            assert_eq!(inode.anextents, 0);
            assert!(attrs.is_empty());
        }
        attrs
    };
    for action in 0..7 {
        let attrs = mutate(action);
        check_kernel(&volume, &attrs, "");
    }
}

#[test]
fn invalid_and_missing_operations_leave_the_inode_unchanged() {
    let volume = fresh("errors.img");
    let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap())).unwrap();
    let ino = fs.superblock().rootino;
    let before = fs.read_inode_raw(ino).unwrap().1;
    assert_eq!(
        fs.remove_xattr(ino, b"user.absent"),
        Err(fs_xfs::Error::NotFound)
    );
    assert_eq!(
        fs.set_xattr(ino, b"user.absent", b"x", XattrMode::Replace),
        Err(fs_xfs::Error::NotFound)
    );
    for name in [
        b"user.".as_slice(),
        b"user.a\0b",
        b"system.posix_acl_access",
        b"trusted.SGI_ACL_FILE",
        &[b'a'; 256],
    ] {
        assert!(matches!(
            fs.set_xattr(ino, name, b"x", XattrMode::Set),
            Err(fs_xfs::Error::UnsupportedFeature(_))
        ));
    }
    assert!(fs
        .set_xattr(ino, b"user.large", &[0; 65537], XattrMode::Set)
        .is_err());
    assert_eq!(fs.read_inode_raw(ino).unwrap().1, before);
    let ro = Filesystem::mount(Arc::new(FileDevice::open(volume.path()).unwrap())).unwrap();
    assert_eq!(
        ro.set_xattr(ino, b"user.a", b"x", XattrMode::Set),
        Err(fs_xfs::Error::ReadOnly)
    );
    assert_eq!(
        ro.remove_xattr(ino, b"user.a"),
        Err(fs_xfs::Error::ReadOnly)
    );
    check_kernel(&volume, &[], "");
}

#[test]
fn c_abi_preserves_binary_names_values_and_buffer_contracts() {
    use fs_xfs::capi::*;
    use std::{ffi::CString, ptr};
    let volume = fresh("c-abi.img");
    let path = CString::new(volume.path().to_str().unwrap()).unwrap();
    let root = c"/";
    let name = CString::new(b"user.\xff.".to_vec()).unwrap();
    let missing = c"user.missing";
    let empty = c"user.empty";
    unsafe {
        let fs = fs_xfs_mount_rw(path.as_ptr());
        assert!(!fs.is_null());
        assert_eq!(
            fs_xfs_setxattr(fs, root.as_ptr(), empty.as_ptr(), ptr::null(), 0, 0),
            0
        );
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), empty.as_ptr(), ptr::null_mut(), 0),
            0
        );
        let bytes = [0u8, 255, 1];
        assert_eq!(
            fs_xfs_setxattr(
                fs,
                root.as_ptr(),
                name.as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                1
            ),
            0
        );
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), name.as_ptr(), ptr::null_mut(), 0),
            3
        );
        let mut out = [0xa5u8; 3];
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), name.as_ptr(), out.as_mut_ptr().cast(), 2),
            -1
        );
        assert_eq!(fs_xfs_last_errno(), 34);
        assert_eq!(out, [0xa5; 3]);
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), name.as_ptr(), ptr::null_mut(), 3),
            -1
        );
        assert_eq!(fs_xfs_last_errno(), 22);
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), name.as_ptr(), out.as_mut_ptr().cast(), 3),
            3
        );
        assert_eq!(out, bytes);
        assert_eq!(fs_xfs_last_errno(), 0);
        assert_eq!(
            fs_xfs_setxattr(fs, root.as_ptr(), name.as_ptr(), ptr::null(), 0, 1),
            -1
        );
        assert_eq!(fs_xfs_last_errno(), 17);
        assert_eq!(
            fs_xfs_setxattr(fs, root.as_ptr(), missing.as_ptr(), ptr::null(), 0, 2),
            -1
        );
        assert_eq!(
            fs_xfs_last_errno(),
            if cfg!(target_os = "macos") { 93 } else { 61 }
        );
        let size = fs_xfs_listxattr(fs, root.as_ptr(), ptr::null_mut(), 0);
        let mut names = vec![0xa5; size as usize];
        assert_eq!(
            fs_xfs_listxattr(
                fs,
                root.as_ptr(),
                names.as_mut_ptr().cast(),
                names.len() - 1
            ),
            -1
        );
        assert!(names.iter().all(|&b| b == 0xa5));
        assert_eq!(
            fs_xfs_listxattr(fs, root.as_ptr(), names.as_mut_ptr().cast(), names.len()),
            size
        );
        assert!(names.split(|&b| b == 0).any(|n| n == name.as_bytes()));
        assert_eq!(fs_xfs_removexattr(fs, root.as_ptr(), name.as_ptr()), 0);
        assert_eq!(
            fs_xfs_getxattr(fs, root.as_ptr(), name.as_ptr(), ptr::null_mut(), 0),
            -1
        );
        assert_eq!(
            fs_xfs_last_errno(),
            if cfg!(target_os = "macos") { 93 } else { 61 }
        );
        fs_xfs_umount(fs);
    }
    check_kernel(
        &volume,
        &[fs_xfs::attr::Xattr {
            name: b"user.empty".to_vec(),
            value: vec![],
        }],
        "",
    );
}

#[test]
fn file_extent_arrays_and_btree_roots_preserve_contents() {
    let volume = fresh("file-data.img");
    check_kernel(
        &volume,
        &[],
        r#"
for name, count in [('extents', 4), ('tree', 40)]:
    fd = os.open(path + '/' + name, os.O_CREAT | os.O_RDWR, 0o600)
    for i in range(count):
        os.pwrite(fd, bytes([i]) * 4096, i * 8192)
        os.fsync(fd)
    os.close(fd)
"#,
    );
    {
        let fs =
            Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap())).unwrap();
        for (name, count, format) in [
            ("extents", 4, fs_xfs::inode::Format::Extents),
            ("tree", 40, fs_xfs::inode::Format::Btree),
        ] {
            let file = fs.lookup_path(&format!("/{name}")).unwrap();
            assert_eq!(file.format, format);
            fs.set_xattr(file.ino, b"user.payload", &[0x7b; 65536], XattrMode::Create)
                .unwrap();
            let (file, raw) = fs.read_inode_raw(file.ino).unwrap();
            for i in 0..count {
                let mut bytes = [0; 4096];
                assert_eq!(fs.read_at(&file, &raw, i * 8192, &mut bytes).unwrap(), 4096);
                assert!(
                    bytes.iter().all(|&b| b == i as u8),
                    "{name}: data block {i} changed"
                );
            }
        }
    }
    // Replay the uncheckpointed inode root through the driver's reader as
    // well as the kernel, including capacity-based bmbt pointer offsets.
    {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(volume.path()).unwrap())).unwrap();
        for (name, count) in [("extents", 4), ("tree", 40)] {
            let file = fs.lookup_path(&format!("/{name}")).unwrap();
            let (inode, raw) = fs.read_inode_raw(file.ino).unwrap();
            assert_eq!(
                fs.get_xattr(&inode, &raw, b"user.payload")
                    .unwrap()
                    .unwrap()
                    .len(),
                65536
            );
            for i in 0..count {
                let mut bytes = [0; 4096];
                assert_eq!(
                    fs.read_at(&inode, &raw, i * 8192, &mut bytes).unwrap(),
                    4096
                );
                assert!(bytes.iter().all(|&b| b == i as u8));
            }
        }
    }
    check_kernel(
        &volume,
        &[],
        r#"
for name, count in [('extents', 4), ('tree', 40)]:
    file = path + '/' + name
    assert os.getxattr(file, b'user.payload') == bytes([0x7b]) * 65536
    with open(file, 'rb') as f:
        for i in range(count):
            f.seek(i * 8192)
            assert f.read(4096) == bytes([i]) * 4096, (name, i)
"#,
    );
}

#[test]
fn device_inodes_can_own_remote_attributes() {
    let volume = fresh("device.img");
    check_kernel(
        &volume,
        &[],
        "import stat\nos.mknod(path + '/device', stat.S_IFCHR | 0o600, os.makedev(1, 3))",
    );
    {
        let fs =
            Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap())).unwrap();
        let device = fs.lookup_path("/device").unwrap();
        fs.set_xattr(
            device.ino,
            b"trusted.payload",
            &[3; 65536],
            XattrMode::Create,
        )
        .unwrap();
        let (inode, raw) = fs.read_inode_raw(device.ino).unwrap();
        assert_eq!(inode.format, fs_xfs::inode::Format::Dev);
        assert_eq!(
            fs.get_xattr(&inode, &raw, b"trusted.payload")
                .unwrap()
                .unwrap()
                .len(),
            65536
        );
    }
    check_kernel(&volume, &[], "assert os.getxattr(path + '/device', b'trusted.payload') == bytes([3]) * 65536\nassert os.stat(path + '/device').st_rdev == os.makedev(1, 3)");
}

#[test]
fn written_attributes_survive_kernel_recovery() {
    let volume = scratch::Volume::empty("xattr_write_oracle", "roundtrip.img", 300 << 20);
    let made = oracle("mkfs.xfs")
        .args(["-q", "-f"])
        .arg(volume.path())
        .output();
    assert!(made.ok(), "{}{}", made.stdout, made.stderr);
    {
        let fs =
            Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap())).unwrap();
        let ino = fs.superblock().rootino;
        fs.set_xattr(ino, b"user.test", b"value", XattrMode::Set)
            .unwrap();
        let (inode, raw) = fs.read_inode_raw(ino).unwrap();
        assert_eq!(
            fs.get_xattr(&inode, &raw, b"user.test").unwrap(),
            Some(b"value".to_vec())
        );
    }
    let output = kernel_run(&format!(
        r#"
        set -eu
        img=$(mktemp /var/tmp/xattr-389-XXXXXX.img)
        cp --sparse=always {source} "$img"
        m=$(mktemp -d)
        mount -o loop,nouuid "$img" "$m"
        test "$(getfattr --only-values -n user.test "$m" 2>/dev/null)" = value
        umount "$m" || {{ sleep 2; umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}; }}
        cp --sparse=always "$img" {source}
        rm "$img"
        rmdir "$m"
        echo DONE
    "#,
        source = volume.guest()
    ));
    assert!(output.contains("DONE"));
    common::assert_xfs_repair_clean(volume.path().to_str().unwrap(), "written attributes");
}

#[test]
fn remote_attribute_blocks_are_charged_and_released_by_quota() {
    let volume = fresh("quota.img");
    kernel_run(&format!(
        r#"
        set -eu
        m=$(mktemp -d)
        mount -o loop,nouuid,uquota {source} "$m"
        touch "$m/file"
        chown 65534 "$m/file"
        umount "$m"
        rmdir "$m"
        echo DONE
    "#,
        source = volume.guest()
    ));
    for remote in [true, false] {
        {
            let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(volume.path()).unwrap()))
                .unwrap();
            let file = fs.lookup_path("/file").unwrap();
            if remote {
                fs.set_xattr(file.ino, b"user.payload", &[0x7b; 65536], XattrMode::Create)
                    .unwrap();
            } else {
                fs.remove_xattr(file.ino, b"user.payload").unwrap();
            }
        }
        let report = kernel_run(&format!(
            r##"
            set -eu
            m=$(mktemp -d)
            mount -o loop,nouuid,uquota {source} "$m"
            xfs_quota -x -c 'report -u -b -n' "$m" | awk '$1 == "#65534" {{print "BLOCKS", $2}}'
            umount "$m"
            rmdir "$m"
            echo DONE
        "##,
            source = volume.guest()
        ));
        let expected = if remote { "BLOCKS 72" } else { "BLOCKS 0" };
        assert!(
            report.lines().any(|line| line.trim() == expected),
            "expected {expected}: {report}"
        );
    }
    common::assert_xfs_repair_clean(
        volume.path().to_str().unwrap(),
        "attribute quota accounting",
    );
}
