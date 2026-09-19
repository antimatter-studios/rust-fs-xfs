//! A name no directory entry can hold is refused before anything is logged
//! (#192).
//!
//! The kernel's `xfs_dir2_namecheck` refuses a name containing `/` or a NUL
//! byte, and the VFS never passes `.` or `..` to be created. `xfs_repair`
//! reports an entry holding either byte as an illegal character.
//! `rename_in_directory` checked only the name's length and wrote whatever
//! it was given into the record. `create_file` checked `/`, `.` and `..`,
//! but not NUL.
//!
//! A refusal must also leave the mount's one checkpoint unspent, so a
//! valid rename afterwards still goes through. `mkfs.xfs -p` makes the
//! image, in the fs-linux-test-harness guest, which always has it — the
//! refusal this covers is the driver's, and a run that did not build the
//! image has not observed it.

use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::oracle;

#[test]
fn a_name_no_entry_can_hold_is_refused() {
    // The protofile, the files it names and the image are all made under
    // `std::env::temp_dir()`, which `scripts/with-test-temp.sh` points at
    // a directory inside this repository — the one tree the guest running
    // mkfs.xfs can see.
    let root = std::env::temp_dir().join(format!("fs_xfs_entry_names_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("empty"), b"").unwrap();
    std::fs::write(
        root.join("proto"),
        format!(
            "/dev/null\n0 0\nd--755 0 0\nf ---644 0 0 {}\n$\n",
            root.join("empty").display()
        ),
    )
    .unwrap();
    let image = root.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(300 * 1024 * 1024))
        .unwrap();
    let out = oracle("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(root.join("proto"))
        .arg(&image)
        .output();
    assert!(out.ok(), "{}{}", out.stdout, out.stderr);

    let dev = Arc::new(FileDevice::open_rw(image.to_str().unwrap()).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount rw");
    let dir = fs.lookup_path("/").expect("the root").ino;

    for bad in [&b"a/b"[..], b".", b"..", b"a\0b", b""] {
        let renamed = fs.rename_in_directory(dir, b"f", bad);
        assert!(
            renamed.is_err(),
            "renamed to {:?}: {renamed:?}",
            String::from_utf8_lossy(bad)
        );
    }
    let created = fs.create_file(dir, b"a\0b", 0o100644);
    assert!(created.is_err(), "created a\\0b: {created:?}");

    fs.rename_in_directory(dir, b"f", b"g")
        .expect("a refused name spent the mount's checkpoint");
    let _ = std::fs::remove_dir_all(&root);
}
