//! POSIX ACLs the kernel set read back as the kernel stored them (#244).
//!
//! XFS does not keep an ACL under `system.posix_acl_access`. That name
//! belongs to the VFS. On disk the ACL is a root-namespace attribute,
//! `SGI_ACL_FILE` (`SGI_ACL_DEFAULT` for a directory's default ACL), and its
//! value is XFS's own `struct xfs_acl`, not the VFS's
//! `posix_acl_xattr_header`: a big-endian entry count, then twelve bytes per
//! entry (tag, id, permissions, two bytes of padding). The kernel translates
//! between the two in `xfs_acl.c`. So this driver reports an ACL as
//! `trusted.SGI_ACL_FILE`, and until now no suite had ever set one.
//!
//! The kernel sets the ACLs with `setfacl`, including one it applies itself
//! by inheritance from a directory's default ACL. Each is then compared two
//! ways:
//!
//! - **the value bytes**, against `getfattr -e hex -n trusted.SGI_ACL_*` run
//!   as root, which returns the stored value untranslated; and
//! - **what the bytes mean**, by decoding `struct xfs_acl` here and
//!   comparing the entries with `getfacl -n`, the kernel's own translation.
//!   This is the check that the value's format is understood, and not only
//!   carried.
//!
//! The entry counts are chosen so the attribute fork is in three shapes:
//! short form inside the inode, a leaf block with the value local, and a
//! leaf with the value in remote blocks. `xfs_db` reports the fork format,
//! and `xfs_repair -n` has to accept the volume.
//!
//! This driver does not write attributes, so the other direction, an ACL
//! this crate writes and the kernel honours, has nothing to test yet.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "acl_oracle";

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// `ACL_USER_OBJ` and the rest, as `struct xfs_acl_entry`'s `ae_tag` holds
/// them (`include/linux/posix_acl.h`), with the word `getfacl` prints.
const TAGS: [(u32, &str); 6] = [
    (0x01, "user"),
    (0x02, "user"),
    (0x04, "group"),
    (0x08, "group"),
    (0x10, "mask"),
    (0x20, "other"),
];

/// `ACL_USER` and `ACL_GROUP`: the tags whose `ae_id` names someone.
const NAMED: [u32; 2] = [0x02, 0x08];

/// `struct xfs_acl`, decoded, as the lines `getfacl -n` prints for it:
/// `user::rw-`, `user:1001:r--`, `mask::rwx` and so on.
fn decode_xfs_acl(value: &[u8], what: &str) -> Vec<String> {
    assert!(
        value.len() >= 4,
        "{what}: {} bytes is no xfs_acl",
        value.len()
    );
    let count = u32::from_be_bytes(value[0..4].try_into().unwrap()) as usize;
    assert_eq!(
        value.len(),
        4 + 12 * count,
        "{what}: acl_cnt says {count} entries, which is {} bytes, and the value is {}",
        4 + 12 * count,
        value.len()
    );
    value[4..]
        .chunks_exact(12)
        .map(|e| {
            let tag = u32::from_be_bytes(e[0..4].try_into().unwrap());
            let id = u32::from_be_bytes(e[4..8].try_into().unwrap());
            let perm = u16::from_be_bytes(e[8..10].try_into().unwrap());
            let word = TAGS
                .iter()
                .find(|(t, _)| *t == tag)
                .unwrap_or_else(|| panic!("{what}: ae_tag {tag:#x} is no ACL tag"))
                .1;
            let who = if NAMED.contains(&tag) {
                id.to_string()
            } else {
                String::new()
            };
            let bits = [(4, 'r'), (2, 'w'), (1, 'x')]
                .iter()
                .map(|&(b, c)| if perm & b != 0 { c } else { '-' })
                .collect::<String>();
            format!("{word}:{who}:{bits}")
        })
        .collect()
}

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

/// One ACL to check: the path in the volume, which of the two it is, and
/// the attribute-fork format `xfs_db` must report for the inode.
struct Case {
    path: &'static str,
    /// `SGI_ACL_FILE` or `SGI_ACL_DEFAULT`.
    attr: &'static str,
    /// `access` or `default`, as `getfacl` names them.
    kind: &'static str,
    /// `core.aformat` as `xfs_db` prints it.
    aformat: &'static str,
}

const CASES: [Case; 5] = [
    // A handful of entries: short form, inline in the inode.
    Case {
        path: "small",
        attr: "SGI_ACL_FILE",
        kind: "access",
        aformat: "1 (local)",
    },
    // A hundred named users: 1,252 bytes, too big for the inode and under
    // the 2,032 a 4 KiB leaf keeps locally
    // (`xfs_attr_leaf_entsize_local_max`), so a leaf block with the value
    // local.
    Case {
        path: "leaf",
        attr: "SGI_ACL_FILE",
        kind: "access",
        aformat: "2 (extents)",
    },
    // A thousand: 12,052 bytes, past what a leaf keeps locally, so the
    // value is in remote blocks.
    Case {
        path: "remote",
        attr: "SGI_ACL_FILE",
        kind: "access",
        aformat: "2 (extents)",
    },
    // A directory's default ACL, and its own access ACL beside it.
    Case {
        path: "dir",
        attr: "SGI_ACL_DEFAULT",
        kind: "default",
        aformat: "1 (local)",
    },
    // A file the kernel created in that directory, whose access ACL it
    // made from the directory's default.
    Case {
        path: "dir/inherited",
        attr: "SGI_ACL_FILE",
        kind: "access",
        aformat: "1 (local)",
    },
];

#[test]
fn acls_the_kernel_set_read_back_as_it_stored_them() {
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volume
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volume beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let name = scratch.guest();

    let dumps: String = CASES
        .iter()
        .map(|c| {
            let p = c.path;
            let a = c.attr;
            let k = c.kind;
            let flag = if k == "default" { "-d" } else { "" };
            format!(
                r#"
        echo "INO {p} $(stat -c %i "$m/{p}")"
        echo "HEX {p} {a} $(getfattr --absolute-names -e hex --only-values -n trusted.{a} "$m/{p}" | od -An -tx1 | tr -d ' \n')"
        getfacl -n -c -p -E {flag} "$m/{p}" | grep -v '^$' | sed "s|^|ACL {p} {k} |"
        "#
            )
        })
        .collect();

    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        users() {{ seq -f "u:%g:r" 2000 $((2000 + $1 - 1)) | paste -sd, -; }}
        : > "$m/small" && setfacl -m u:1001:rw,g:1002:r "$m/small"
        : > "$m/leaf" && setfacl -m "$(users 100)" "$m/leaf"
        : > "$m/remote" && setfacl -m "$(users 1000)" "$m/remote"
        mkdir "$m/dir" && setfacl -d -m u:1003:rwx,g:1004:rx "$m/dir"
        : > "$m/dir/inherited"
        echo SET_OK
        {dumps}
        # RETRIED ONCE. A busy unmount under a loaded runner is ordinary and
        # clears in a moment; one that does not leaves a volume xfs_db would
        # read mid-flight.
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ));
    for step in ["MKFS_OK", "MOUNT_OK", "SET_OK"] {
        assert!(
            built.contains(step),
            "building the volume failed before {step}:\n{built}"
        );
    }
    assert!(
        !built.contains("UMOUNT_FAILED"),
        "the volume could not be unmounted:\n{built}"
    );
    repair::assert_agreed(&built, "the volume the kernel set ACLs on");

    let dev = FileDevice::open(scratch.path()).expect("open the volume");
    let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");

    let mut shapes = String::new();
    for c in &CASES {
        let what = format!("{} {}", c.path, c.kind);
        let ino: u64 = built
            .lines()
            .find_map(|l| l.strip_prefix(&format!("INO {} ", c.path)))
            .unwrap_or_else(|| panic!("{what}: the guest did not report the inode:\n{built}"))
            .trim()
            .parse()
            .expect("an inode number");
        let stored = unhex(
            built
                .lines()
                .find_map(|l| l.strip_prefix(&format!("HEX {} {} ", c.path, c.attr)))
                .unwrap_or_else(|| panic!("{what}: the guest did not report the value:\n{built}")),
        );
        let getfacl: Vec<String> = built
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("ACL {} {} ", c.path, c.kind)))
            .map(|l| l.trim().trim_start_matches("default:").to_string())
            .collect();
        assert!(
            !getfacl.is_empty(),
            "{what}: getfacl printed nothing:\n{built}"
        );

        let (inode, raw) = fs.read_inode_raw(ino).expect("read the inode");
        assert_eq!(
            fs.lookup_path(&format!("/{}", c.path))
                .expect("look it up")
                .ino,
            ino,
            "{what}: the path resolves to another inode"
        );
        let name = format!("trusted.{}", c.attr);
        let value = fs
            .get_xattr(&inode, &raw, name.as_bytes())
            .unwrap_or_else(|e| panic!("{what}: reading {name}: {e}"))
            .unwrap_or_else(|| panic!("{what}: the driver finds no {name}"));

        // THE BYTES, as the kernel stored them.
        assert_eq!(
            value.len(),
            stored.len(),
            "{what}: {name} is {} bytes to the driver and {} to the kernel",
            value.len(),
            stored.len()
        );
        assert!(
            value == stored,
            "{what}: {name}'s bytes differ from the kernel's"
        );

        // AND WHAT THEY MEAN, against the kernel's translation.
        let mut decoded = decode_xfs_acl(&value, &what);
        let mut expected = getfacl.clone();
        decoded.sort();
        expected.sort();
        assert_eq!(
            decoded, expected,
            "{what}: the entries decoded from {name} are not the ones getfacl reports"
        );

        shapes.push_str(&format!(
            "{} ino {ino} {} entries, {} bytes\n",
            c.path,
            decoded.len(),
            value.len()
        ));
    }

    // THE SHAPES, from the reference tool. Asked after the comparison so a
    // shape that came out different still reports what was compared.
    let script: String = CASES
        .iter()
        .map(|c| {
            let ino = shapes
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{} ino ", c.path)))
                .and_then(|l| l.split_whitespace().next())
                .expect("recorded above")
                .to_string();
            format!(
                "echo \"AFORMAT {} $(xfs_db -r -c 'inode {ino}' -c 'print core.aformat' {name})\"\n",
                c.path
            )
        })
        .collect();
    let out = kernel_run(&format!("{script}\necho DONE\n"));
    for c in &CASES {
        let got = out
            .lines()
            .find_map(|l| l.strip_prefix(&format!("AFORMAT {} ", c.path)))
            .unwrap_or_else(|| panic!("{}: xfs_db did not answer:\n{out}", c.path))
            .trim()
            .trim_start_matches("core.aformat = ");
        assert_eq!(
            got, c.aformat,
            "{}: the attribute fork is not the shape this case exists to cover\n{shapes}",
            c.path
        );
    }
    // The leaf and remote cases share a fork format, and which of the two
    // a value is depends on its size against the 2,032 bytes a 4 KiB leaf
    // keeps locally. Their sizes are what say they landed either side.
    for (case, entries, bytes) in [("leaf", 104, 1252), ("remote", 1004, 12052)] {
        let line = shapes
            .lines()
            .find(|l| l.starts_with(&format!("{case} ")))
            .expect("recorded above");
        assert!(
            line.ends_with(&format!("{entries} entries, {bytes} bytes")),
            "the {case} case is not the ACL it was meant to be: {line}"
        );
    }
}
