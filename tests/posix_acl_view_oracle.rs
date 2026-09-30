//! Every attribute root's `listxattr` reports, this driver reports too,
//! POSIX ACLs included, with the same bytes (#285).
//!
//! XFS keeps a POSIX ACL as a root-namespace attribute, `SGI_ACL_FILE` or
//! `SGI_ACL_DEFAULT`, holding its own `struct xfs_acl`. The kernel shows
//! it to the VFS as `system.posix_acl_access` / `system.posix_acl_default`
//! in the VFS's `posix_acl_xattr_header` format, translating in
//! `fs/xfs/xfs_acl.c`, and `xfs_xattr_put_listent` lists that name to
//! everyone, and the untranslated `trusted.` name beside it to root.
//!
//! So the reference here is the whole listing `getfattr -d -m - -e hex`
//! prints as root, in the guest, over a volume the kernel set the ACLs
//! on: every name and every value, the translated ACLs and the stored
//! ones, compared with `list_xattrs` and with `get_xattr` by name. The
//! volume is made twice, v5 and v4, with ACLs sized for the shapes an
//! attribute fork has: short form, a leaf with the value local, and, on
//! v5 only, a leaf with the value in remote blocks. `xfs_repair -n` has to
//! accept each volume first.

mod common;

use common::{kernel_run, repair, scratch, share};
use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Where this suite's scratch volumes live, under `.vm-share/scratch/`,
/// out of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "posix_acl_view_oracle";

/// The shell that sets the attributes, on the volume mounted at `$m`.
///
/// `small` is short form inside the inode. `leaf` and `remote` get `leaf`
/// and `remote` named users, four entries more with the owner, group,
/// mask and other. On v5 that is 1,252 bytes, which a 4 KiB leaf keeps
/// locally, and 12,052, past the 2,032 it keeps locally, so in remote
/// blocks. A v4 filesystem holds no ACL of more than 25 entries
/// (`XFS_ACL_MAX_ENTRIES`), so there both are leaves. `dir` has a default
/// ACL and an access ACL, and `dir/inherited` was given its ACL by the
/// kernel from that default. `mixed` carries a user attribute beside its
/// ACL, and `plain` carries nothing.
fn setup(leaf: u32, remote: u32) -> String {
    format!(
        r#"
        users() {{ seq -f "u:%g:r" 2000 $((2000 + $1 - 1)) | paste -sd, -; }}
        : > "$m/plain"
        : > "$m/small" && setfacl -m u:1001:rw,g:1002:r "$m/small"
        : > "$m/leaf" && setfacl -m "$(users {leaf})" "$m/leaf"
        : > "$m/remote" && setfacl -m "$(users {remote})" "$m/remote"
        mkdir "$m/dir" && setfacl -m u:1005:rx "$m/dir" && setfacl -d -m u:1003:rwx,g:1004:rx "$m/dir"
        : > "$m/dir/inherited"
        : > "$m/mixed" && setfattr -n user.colour -v blue "$m/mixed" && setfacl -m g:1006:w "$m/mixed"
        echo SET_OK
"#
    )
}

const PATHS: [&str; 7] = [
    "plain",
    "small",
    "leaf",
    "remote",
    "dir",
    "dir/inherited",
    "mixed",
];

/// Hex as `getfattr -e hex` prints it (`0x0000...`), as bytes.
fn unhex(text: &str) -> Vec<u8> {
    let digits = text.trim().trim_start_matches("0x");
    assert!(
        digits.len().is_multiple_of(2),
        "odd hex from getfattr: {text}"
    );
    (0..digits.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&digits[i..i + 2], 16).expect("hex"))
        .collect()
}

/// `getfattr -d`'s listing for one path, as name to value.
fn listing(out: &str, path: &str) -> BTreeMap<Vec<u8>, Vec<u8>> {
    let begin = format!("LIST_BEGIN {path}");
    let end = format!("LIST_END {path}");
    let lines: Vec<&str> = out
        .lines()
        .skip_while(|l| l.trim() != begin)
        .skip(1)
        .take_while(|l| l.trim() != end)
        .collect();
    assert!(
        out.lines().any(|l| l.trim() == end),
        "{path}: the guest printed no listing:\n{out}"
    );
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| match l.split_once('=') {
            Some((name, value)) => (name.as_bytes().to_vec(), unhex(value)),
            None => (l.as_bytes().to_vec(), Vec::new()),
        })
        .collect()
}

fn printable(map: &BTreeMap<Vec<u8>, Vec<u8>>) -> String {
    map.iter()
        .map(|(k, v)| format!("  {} ({} bytes)\n", String::from_utf8_lossy(k), v.len()))
        .collect()
}

/// Make a volume with `mkfs_options`, have the kernel set the ACLs, and
/// compare what root lists with what the driver lists.
fn check(scratch: &scratch::Volume, version: &str, mkfs_options: &str, (leaf, remote): (u32, u32)) {
    let name = scratch.guest();
    let lists: String = PATHS
        .iter()
        .map(|p| {
            format!(
                "echo \"INO {p} $(stat -c %i \"$m/{p}\")\"\n\
                 echo \"LIST_BEGIN {p}\"\n\
                 getfattr --absolute-names -d -m - -e hex \"$m/{p}\"\n\
                 echo \"LIST_END {p}\"\n"
            )
        })
        .collect();
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {mkfs_options} {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        {setup}
        {lists}
        # RETRIED ONCE. A busy unmount under a loaded runner is ordinary and
        # clears in a moment; one that does not leaves a volume the driver
        # would read mid-flight.
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        {repair}
        echo DONE
        "#,
        setup = setup(leaf, remote),
        repair = repair::script(&name),
    ));
    for step in ["MKFS_OK", "MOUNT_OK", "SET_OK"] {
        assert!(
            built.contains(step),
            "{version}: building the volume failed before {step}:\n{built}"
        );
    }
    repair::assert_agreed(
        &built,
        &format!("the {version} volume the kernel set ACLs on"),
    );

    let dev = FileDevice::open(scratch.path()).expect("open the volume");
    let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");
    assert_eq!(
        fs.superblock().is_v5(),
        version == "v5",
        "{version}: mkfs made the other version"
    );

    let mut acls = 0;
    for path in PATHS {
        let what = format!("{version} {path}");
        let ino: u64 = built
            .lines()
            .find_map(|l| l.strip_prefix(&format!("INO {path} ")))
            .unwrap_or_else(|| panic!("{what}: the guest did not report the inode:\n{built}"))
            .trim()
            .parse()
            .expect("an inode number");
        let expected = listing(&built, path);
        let (inode, raw) = fs.read_inode_raw(ino).expect("read the inode");
        let got: BTreeMap<Vec<u8>, Vec<u8>> = fs
            .list_xattrs(&inode, &raw)
            .unwrap_or_else(|e| panic!("{what}: listing its attributes: {e}"))
            .into_iter()
            .map(|a| (a.name, a.value))
            .collect();
        assert!(
            got == expected,
            "{what}: the driver lists\n{}and root, through the kernel, lists\n{}",
            printable(&got),
            printable(&expected)
        );
        for (attr, value) in &expected {
            assert_eq!(
                fs.get_xattr(&inode, &raw, attr)
                    .unwrap_or_else(|e| panic!("{what}: get_xattr: {e}"))
                    .as_ref(),
                Some(value),
                "{what}: get_xattr({}) disagrees with the listing",
                String::from_utf8_lossy(attr)
            );
            if attr.starts_with(b"system.posix_acl_") {
                acls += 1;
            }
        }
    }
    // small, leaf, remote, mixed, dir/inherited: one each. dir: two.
    assert_eq!(
        acls, 7,
        "{version}: the volume does not carry the ACLs this suite exists to compare"
    );
}

#[test]
fn every_attribute_root_lists_is_listed_with_its_bytes() {
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volumes
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volumes beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
    // BOTH MADE BEFORE EITHER IS USED. The guest caches what it has seen
    // of the shared directory, and a volume made after the other's removal
    // emptied it was, measured, not there for the guest to format.
    let volume = |version: &str| {
        scratch::Volume::empty(
            SUITE,
            &format!("{version}-{}.img", std::process::id()),
            400 * 1024 * 1024,
        )
    };
    let (v5, v4) = (volume("v5"), volume("v4"));
    check(&v5, "v5", "", (100, 1000));
    check(&v4, "v4", "-m crc=0,finobt=0", (20, 21));
}
