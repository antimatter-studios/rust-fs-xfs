//! The public extent-size flag recognises hints set by Linux (#533).
//!
//! xfs_io sets and reports byte-sized hints on empty files. After unmount,
//! xfs_db reports the on-disk flags and block-sized hint independently of
//! this parser. Plain, cleared and immutable files are negative controls.

mod common;

use common::{guest_quote, kernel_run, scratch};
use fs_core::FileDevice;
use fs_xfs::inode::{flags, offsets};
use fs_xfs::Filesystem;
use std::sync::Arc;

#[test]
fn extent_size_hints_match_xfsprogs_and_do_not_select_immutable_files() {
    let volume = scratch::Volume::empty(
        "inode_extsize_kernel",
        &format!("{}-hints.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let out = kernel_run(&format!(
        r#"
        set -eu
        work=$(mktemp -d /var/tmp/inode-extsize-XXXXXX)
        img="$work/volume.img"
        m="$work/mnt"
        cleanup() {{
            if mountpoint -q "$m"; then
                umount "$m" || {{ echo UMOUNT_FAILED; return 1; }}
            fi
            rm -rf "$work"
        }}
        trap cleanup EXIT
        truncate -s 400M "$img"
        mkfs.xfs -q -f "$img"
        mkdir "$m"
        mount -o loop "$img" "$m"
        touch "$m/plain"
        xfs_io -f -c 'extsize 65536' "$m/hint64"
        xfs_io -f -c 'extsize 131072' "$m/hint128"
        xfs_io -f -c 'extsize 65536' -c 'extsize 0' "$m/cleared"
        xfs_io -f -c 'chattr +i' "$m/immutable"
        for f in plain hint64 hint128 cleared immutable; do
            ino=$(stat -c %i "$m/$f")
            printf 'INO %s %s\n' "$f" "$ino"
            printf 'HINT %s ' "$f"
            xfs_io -r -c stat "$m/$f" | sed -n 's/^fsxattr.extsize = //p'
        done
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        for f in plain hint64 hint128 cleared immutable; do
            echo "DB_BEGIN $f"
            xfs_db -r -c "path /$f" -c 'print core.flags core.extsize' "$img"
            echo "DB_END $f"
        done
        cp --sparse=always "$img" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(&volume.guest()),
    ));
    let fs = Filesystem::mount(Arc::new(FileDevice::open(volume.path()).unwrap()))
        .expect("mount the kernel-created filesystem");
    for (name, expected_hint) in [
        ("plain", 0),
        ("hint64", 65536),
        ("hint128", 131072),
        ("cleared", 0),
        ("immutable", 0),
    ] {
        let report = |key: &str| -> u64 {
            let prefix = format!("{key} {name} ");
            out.lines()
                .find_map(|line| line.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("missing {key} for {name}:\n{out}"))
                .trim()
                .parse()
                .unwrap_or_else(|error| panic!("invalid {key} for {name}: {error}\n{out}"))
        };
        let db = |field: &str| -> u32 {
            let prefix = format!("core.{field} = ");
            let begin = format!("DB_BEGIN {name}");
            let end = format!("DB_END {name}");
            let value = out
                .lines()
                .skip_while(|line| *line != begin)
                .take_while(|line| *line != end)
                .find_map(|line| line.strip_prefix(&prefix))
                .unwrap_or_else(|| panic!("xfs_db reported no {field} for {name}:\n{out}"))
                .trim();
            if let Some(hex) = value.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).expect("xfs_db hexadecimal field")
            } else {
                value.parse().expect("xfs_db decimal field")
            }
        };
        let (inode, raw) = fs.read_inode_raw(report("INO")).expect("parse the inode");
        assert_eq!(inode.size, 0, "{name}: the file must remain empty");
        assert_eq!(report("HINT"), expected_hint, "{name}: xfs_io hint\n{out}");
        assert_eq!(u32::from(inode.flags), db("flags"), "{name}: flags");
        let blocks = u32::from_be_bytes(
            raw[offsets::EXTSIZE..offsets::EXTSIZE + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(blocks, db("extsize"), "{name}: on-disk extent hint");
        assert_eq!(
            u64::from(blocks) * u64::from(fs.superblock().blocksize),
            expected_hint,
            "{name}: the on-disk hint must match xfs_io's bytes"
        );
        assert_eq!(
            inode.flags & flags::EXTSIZE != 0,
            expected_hint != 0,
            "{name}: the public flag must recognise exactly the files with an extent-size hint"
        );
        if name == "immutable" {
            // The A/B measurement must actually exercise the bit the old
            // public constant selected, rather than another plain inode.
            assert_eq!(inode.flags & 0x0008, 0x0008, "immutable control");
        }
    }
}
