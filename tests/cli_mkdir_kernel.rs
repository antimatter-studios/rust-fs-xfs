//! mkdir --parents leaves the same directory tree as Linux's mkdir -p on
//! a twin. xfs_repair grades our image before any kernel can replay it.
mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const LIST: &str = r#"
list() {
    ( cd "$1" && find a sub/new -type d -printf '%p|%m|%n\n' | LC_ALL=C sort )
}
"#;

#[test]
fn parents_match_the_kernel_and_leave_the_log_clean_before_replay() {
    let copy = scratch::Volume::copy_of(
        "cli_mkdir_kernel",
        &fixture("xfscli-v5.img"),
        &format!("{}-parents.img", std::process::id()),
    );
    let image = copy.path().to_str().unwrap();
    let twin = kernel_run(&format!(
        r#"
        {LIST}
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        sync; echo 3 > /proc/sys/vm/drop_caches
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        ( cd "$work/m"; umask 022; mkdir -p a/b/c a/b/d sub/new/leaf )
        list "$work/m"
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(image),
    ));
    for path in ["/a/b/c", "/a/b/d", "/sub/new/leaf"] {
        let report = stdout(&ok(tool("fs.xfs").args([image, "mkdir", "-p", path])));
        assert_eq!(json_field(&report, "path"), path, "{report}");
        let dirty = stdout(&ok(tool("fs.xfs").args([image, "get", "dirty", "--text"])));
        assert_eq!(dirty.trim(), "false", "mkdir -p {path} left a dirty log");
    }
    let ours = kernel_run(&format!(
        r#"
        {LIST}
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        sync; echo 3 > /proc/sys/vm/drop_caches
        cp --sparse=always {image} "$work/image"
        img="$work/image"
        {repair}
        mkdir "$work/m"
        mount -o loop,ro,nouuid "$img" "$work/m"
        echo MOUNT_OK
        list "$work/m"
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    repair::assert_agreed(&ours, "mkdir --parents before any replay");
    assert!(ours.contains("MOUNT_OK"), "{ours}");
    let directories = |out: &str| {
        out.lines()
            .filter(|line| line.contains('|'))
            .map(str::to_string)
            .collect::<Vec<_>>()
    };
    let expected = directories(&twin);
    assert_eq!(expected.len(), 6, "the kernel's mkdir -p tree: {twin}");
    assert_eq!(directories(&ours), expected, "{ours}");
}
