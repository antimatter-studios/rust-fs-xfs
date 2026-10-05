//! `fs.xfs set label` writes the label every reader sees (#341).
//!
//! The label is `sb_fname`, twelve bytes in the superblock, and there is a
//! superblock at the start of every allocation group. The reference tool
//! writes the label into all of them, so after `set label` the reference
//! reader (`xfs_db`) must report it in every AG's superblock, the Linux
//! kernel must report it from a mount, and `xfs_repair -n` must find
//! nothing wrong -- on a v5 volume, where each copy carries a CRC, and on
//! a v4 one, where none does.
//!
//! A label too long for its field, and a volume whose log still holds
//! records, are refused and the image is left as it was.

mod cli_support;
mod common;

use cli_support::*;
use common::{assert_xfs_repair_clean, fixture, guest_quote, kernel_run, oracle};

/// The label in AG `ag`'s superblock, as `xfs_db -r -c 'sb N' -c 'print
/// fname'` prints it: `fname = "CLIV5\000\000..."`, padding stripped.
fn db_label(image: &str, ag: u32) -> String {
    let out = oracle("xfs_db")
        .args(["-r", "-c", &format!("sb {ag}"), "-c", "print fname", image])
        .output();
    assert!(
        out.ok(),
        "xfs_db print fname of sb {ag} failed:\n{}",
        out.stderr
    );
    let raw = out
        .stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("fname = "))
        .unwrap_or_else(|| panic!("xfs_db printed no fname for sb {ag}:\n{}", out.stdout));
    raw.trim_matches('"')
        .split("\\000")
        .next()
        .unwrap_or("")
        .to_string()
}

fn ag_count(image: &str) -> u32 {
    let get = stdout(&ok(tool("fs.xfs").args([
        image,
        "get",
        "xfs.ag_count",
        "--text",
    ])));
    get.trim()
        .parse()
        .unwrap_or_else(|e| panic!("ag_count {get:?}: {e}"))
}

/// The label the kernel reports for `image` from a mount, read with
/// `xfs_io -c label` on a copy on the guest's own disk.
fn kernel_label(image: &str) -> String {
    let out = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        img=$(mktemp -u /tmp/cli-label-XXXXXX.img)
        cp --sparse=always {image} "$img"
        m=$(mktemp -d)
        if mount -o loop,ro,nouuid "$img" "$m"; then
            echo MOUNT_OK
            xfs_io -c label "$m"
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_REFUSED
        fi
        rmdir "$m"
        rm -f "$img"
        echo DONE
        "#,
        image = guest_quote(image),
    ));
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel refused the volume:\n{out}"
    );
    out.lines()
        .find_map(|l| l.trim().strip_prefix("label = "))
        .unwrap_or_else(|| panic!("xfs_io printed no label:\n{out}"))
        .trim_matches('"')
        .to_string()
}

fn relabel_is_read_everywhere(name: &str, label: &str) {
    // One scratch directory per image AND label: the tests run in parallel,
    // and two relabelling the same fixture would otherwise share a copy.
    let tag: String = label
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    let path = scratch_dir(&format!("label-{name}-{tag}")).join(name);
    std::fs::copy(fixture(name), &path).expect("copy the fixture");
    let image = path.to_str().expect("a UTF-8 scratch path");

    ok(tool("fs.xfs").args([image, "set", "label", label]));

    let got = stdout(&ok(tool("fs.xfs").args([image, "get", "label", "--text"])));
    assert_eq!(
        got.trim(),
        label,
        "{name}: fs.xfs reads back a different label"
    );
    for ag in 0..ag_count(image) {
        assert_eq!(
            db_label(image, ag),
            label,
            "{name}: xfs_db reads a different label in AG {ag}'s superblock"
        );
    }
    assert_xfs_repair_clean(image, &format!("{name} after set label"));
    assert_eq!(
        kernel_label(image),
        label,
        "{name}: the kernel reads a different label"
    );
}

#[test]
fn a_new_label_is_the_one_the_kernel_and_the_reference_tools_read_on_v5() {
    relabel_is_read_everywhere("xfscli-v5.img", "DJ RENAMED");
}

#[test]
fn a_new_label_is_the_one_the_kernel_and_the_reference_tools_read_on_v4() {
    relabel_is_read_everywhere("xfscli-v4.img", "DJ RENAMED");
}

#[test]
fn a_label_of_exactly_twelve_bytes_fills_the_field() {
    relabel_is_read_everywhere("xfscli-v5.img", "TWELVE_BYTES");
}

fn refused_and_unchanged(name: &str, label: &str, says: &str) {
    let path = scratch_dir(&format!("label-refused-{name}")).join(name);
    std::fs::copy(fixture(name), &path).expect("copy the fixture");
    let before = std::fs::read(&path).unwrap();
    let out = tool("fs.xfs")
        .arg(&path)
        .args(["set", "label", label])
        .output()
        .expect("spawn fs.xfs");
    assert!(
        !out.status.success(),
        "{name}: set label {label:?} was accepted"
    );
    assert!(
        stderr(&out).contains(says),
        "{name}: the refusal does not say {says:?}: {}",
        stderr(&out)
    );
    assert!(
        std::fs::read(&path).unwrap() == before,
        "{name}: the refused label changed the image"
    );
}

#[test]
fn a_label_too_long_is_refused_and_nothing_changes() {
    refused_and_unchanged("xfscli-v5.img", "THIRTEEN_BYTE", "12");
}

#[test]
fn a_volume_with_a_dirty_log_is_refused_and_nothing_changes() {
    refused_and_unchanged("xfsdirty.img", "DJ", "log");
}
