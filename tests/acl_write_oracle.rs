//! ACLs this driver sets, changes with `chmod` and removes are the ACLs
//! the Linux kernel gives the same files for the same requests (#390).
//!
//! The reference is the kernel's own ACL code, not a string this test
//! writes down: the guest makes every file twice, the driver changes one
//! of each pair, the kernel changes the other with `setfacl`, `chmod` and
//! `setfattr`, and `getfacl` must print the two the same, mode included.
//!
//! - `acl`: an access ACL naming a user, under a mask, then `chmod 750`,
//!   which moves the owner, mask and other entries with the mode;
//! - `equiv`: an access ACL of only the three base entries, which is the
//!   mode and is stored as the mode alone;
//! - `gone`: an access ACL set and then removed, which leaves the mode as
//!   the ACL had made it;
//! - `dir`: a default ACL on a directory.
//!
//! `xfs_repair -n` must call the volume clean afterwards.

mod common;

use common::{kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::acl::{AclEntry, AclKind};
use fs_xfs::format::acl::tag;
use fs_xfs::Filesystem;
use std::sync::Arc;

const SUITE: &str = "acl_write_oracle";

fn e(tag: u32, id: u32, perm: u16) -> AclEntry {
    AclEntry { tag, id, perm }
}

#[test]
fn acls_the_driver_writes_are_the_kernels() {
    let volume = scratch::Volume::empty(SUITE, "acl.img", 300 * 1024 * 1024);
    let image = volume.guest();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        for f in acl equiv gone; do : > "$m/$f"; : > "$m/k-$f"; chmod 644 "$m/$f" "$m/k-$f"; done
        mkdir "$m/dir" "$m/k-dir"
        umount "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );

    {
        let dev = FileDevice::open_rw(volume.path().to_str().unwrap()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = |p: &str| fs.lookup_path(p).expect(p).ino;
        let named = [
            e(tag::USER_OBJ, 0, 6),
            e(tag::USER, 1000, 4),
            e(tag::GROUP_OBJ, 0, 4),
            e(tag::MASK, 0, 6),
            e(tag::OTHER, 0, 0),
        ];
        fs.set_acl(ino("/acl"), AclKind::Access, &named)
            .expect("set acl");
        fs.chmod(ino("/acl"), 0o750).expect("chmod under an acl");
        fs.set_acl(
            ino("/equiv"),
            AclKind::Access,
            &[
                e(tag::USER_OBJ, 0, 6),
                e(tag::GROUP_OBJ, 0, 4),
                e(tag::OTHER, 0, 0),
            ],
        )
        .expect("set an acl the mode says");
        fs.set_acl(ino("/gone"), AclKind::Access, &named)
            .expect("set acl");
        fs.remove_acl(ino("/gone"), AclKind::Access)
            .expect("remove acl");
        fs.set_acl(
            ino("/dir"),
            AclKind::Default,
            &[
                e(tag::USER_OBJ, 0, 7),
                e(tag::USER, 1000, 7),
                e(tag::GROUP_OBJ, 0, 5),
                e(tag::MASK, 0, 7),
                e(tag::OTHER, 0, 5),
            ],
        )
        .expect("set a default acl");
    }

    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            setfacl -m u::rw-,u:1000:r--,g::r--,m::rw-,o::--- "$m/k-acl" && chmod 750 "$m/k-acl"
            setfacl -m u::rw-,g::r--,o::--- "$m/k-equiv"
            setfacl -m u::rw-,u:1000:r--,g::r--,m::rw-,o::--- "$m/k-gone"
            setfattr -x system.posix_acl_access "$m/k-gone"
            setfacl -d -m u::rwx,u:1000:rwx,g::r-x,m::rwx,o::r-x "$m/k-dir"
            for f in acl equiv gone dir; do
                for side in "$f" "k-$f"; do
                    echo "MODE $side $(stat -c %a "$m/$side")"
                    echo "FACL $side $(getfacl -n -p "$m/$side" | grep -v '^#' | tr '\n' ' ')"
                done
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
    repair::assert_agreed(&out, "the volume after the driver's ACL writes");
    let field = |key: &str, side: &str| -> String {
        out.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{key} {side} ")))
            .unwrap_or_else(|| panic!("no {key} for {side}:\n{out}"))
            .trim()
            .to_string()
    };
    for f in ["acl", "equiv", "gone", "dir"] {
        let kernel = format!("k-{f}");
        assert_eq!(field("MODE", f), field("MODE", &kernel), "{f}: mode\n{out}");
        assert_eq!(field("FACL", f), field("FACL", &kernel), "{f}: ACL\n{out}");
    }
}
