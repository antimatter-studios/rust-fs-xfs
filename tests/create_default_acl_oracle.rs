//! A create inside a directory with a default ACL is refused, by name,
//! and a create anywhere else still gives the kernel a usable file (#284).
//!
//! When the kernel creates an inode in a directory carrying a default ACL
//! (`SGI_ACL_DEFAULT` on disk), `posix_acl_create` gives the new inode an
//! access ACL derived from it, and a new directory a copy of it as its own
//! default. This driver has no attribute write path, so a create there
//! would make an inode without the ACL the directory promises its
//! contents. It is refused instead, before anything is logged.
//!
//! The reference is the kernel itself, in the guest: it sets the ACLs,
//! shows that its own create in that directory inherits one (which is
//! why the driver must not pretend to), and afterwards replays what the
//! driver did log. `xfs_repair -n` grades the volume before and after.
//!
//! `xfs_repair` runs on a copy in the guest's own `/tmp`, as
//! `create_replay_oracle` does, not on the shared file itself.

mod common;

use common::{kernel_run, repair, scratch, share};
use fs_core::FileDevice;
use fs_xfs::{Error, Filesystem};
use std::sync::Arc;

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`
/// (#223).
const SUITE: &str = "create_default_acl_oracle";

/// Directories the kernel sets up, and whether a create in each must be
/// refused. `inherit` carries a default ACL. `access` carries only an
/// access ACL, which a create does not inherit. `lookalike` carries a
/// *user* attribute with the default ACL's stored name, which is not an
/// ACL. `plain` carries nothing.
const DIRS: [(&str, bool); 4] = [
    ("inherit", true),
    ("access", false),
    ("lookalike", false),
    ("plain", false),
];

fn inode_of(out: &str, what: &str) -> u64 {
    out.lines()
        .find_map(|l| l.strip_prefix(&format!("INO {what} ")))
        .unwrap_or_else(|| panic!("the guest did not report {what}'s inode:\n{out}"))
        .trim()
        .parse()
        .expect("an inode number")
}

#[test]
fn a_create_under_a_default_acl_is_refused_and_elsewhere_replays_clean() {
    // THE SHARED DIRECTORY IS ALWAYS THERE: `chore fixtures` makes it,
    // and an absent one is that build not having happened.
    assert!(
        share().is_dir(),
        "{} is not there: `chore fixtures` makes the shared directory this suite \
         writes its scratch volume into. Tests never skip on a missing fixture.",
        share().display()
    );
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("v5-{}.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let name = volume.guest();

    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        mkdir "$m/inherit" "$m/access" "$m/lookalike" "$m/plain"
        setfacl -d -m u:1003:rwx,g:1004:rx "$m/inherit" \
            && setfacl -m u:1005:rx "$m/access" \
            && setfattr -n user.SGI_ACL_DEFAULT -v x "$m/lookalike" \
            && echo SET_OK
        # What the kernel does with the same create: the new file inherits.
        : > "$m/inherit/by-kernel"
        getfattr --absolute-names -n system.posix_acl_access "$m/inherit/by-kernel" \
            >/dev/null 2>&1 && echo KERNEL_INHERITED
        for d in inherit access lookalike plain; do echo "INO $d $(stat -c %i "$m/$d")"; done
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {name} "$img"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        repair = repair::script("\"$img\""),
    ));
    for step in ["MKFS_OK", "MOUNT_OK", "SET_OK", "KERNEL_INHERITED", "DONE"] {
        assert!(
            built.contains(step),
            "building the volume failed before {step}:\n{built}"
        );
    }
    assert!(!built.contains("UMOUNT_FAILED"), "{built}");
    repair::assert_agreed(&built, "the volume the kernel set ACLs on");

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        for (dir, refused) in DIRS {
            let parent = inode_of(&built, dir);
            let file = fs.create_file(parent, b"new-file", 0o100644);
            let sub = fs.create_directory(parent, b"new-dir", 0o040755);
            for (what, got) in [("create_file", file), ("create_directory", sub)] {
                match (refused, got) {
                    (true, Err(Error::UnsupportedFeature(why))) => assert!(
                        why.contains("SGI_ACL_DEFAULT"),
                        "{dir}: {what} was refused, but not by naming the default ACL: {why}"
                    ),
                    (true, other) => panic!(
                        "{dir}: {what} under a default ACL must be refused, since the new \
                         inode would not inherit it; got {other:?}"
                    ),
                    (false, Ok(_)) => {}
                    (false, Err(e)) => panic!("{dir}: {what} must be accepted: {e}"),
                }
            }
        }
    }

    let checks: String = DIRS
        .iter()
        .map(|(d, _)| {
            format!(
                r#"
            echo "ENTRIES {d} $(ls -A "$m/{d}" | sort | tr '\n' ' ')"
            for n in new-file new-dir; do
                getfattr --absolute-names -n system.posix_acl_access "$m/{d}/$n" \
                    >/dev/null 2>&1 && echo "ACL {d}/$n"
            done"#
            )
        })
        .collect();
    let replayed = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp {name} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            {checks}
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        else
            echo MOUNT_FAILED
            dmesg | tail -12
        fi
        rmdir "$m"
        {repair}
        rm -f "$img"
        echo DONE
        "#,
        repair = repair::script("\"$img\""),
    ));
    assert!(
        !replayed.contains("MOUNT_FAILED") && !replayed.contains("UMOUNT_FAILED"),
        "the kernel could not replay what the driver logged:\n{replayed}"
    );
    for (dir, refused) in DIRS {
        let want = if refused {
            "by-kernel"
        } else {
            "new-dir new-file"
        };
        let listed = replayed
            .lines()
            .find_map(|l| l.trim().strip_prefix(&format!("ENTRIES {dir}")))
            .unwrap_or_else(|| panic!("{dir}: the guest did not list it:\n{replayed}"))
            .trim();
        assert_eq!(
            listed, want,
            "{dir}: after the replay the kernel lists the wrong entries:\n{replayed}"
        );
    }
    // Nothing the driver made carries an ACL, and none of them should:
    // none of those directories has a default one.
    assert!(
        !replayed.lines().any(|l| l.trim().starts_with("ACL ")),
        "an inode the driver made carries an ACL:\n{replayed}"
    );
    repair::assert_agreed(
        &replayed,
        "the volume after the kernel replayed the creates",
    );
}
