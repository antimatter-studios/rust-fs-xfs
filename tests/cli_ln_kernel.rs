//! The installed ln's metadata is judged by xfs_repair and Linux, before
//! any replay can hide an unclean release. The format-limit fixture is
//! made by xfs_db, and the real kernel independently refuses another link.
mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_ln_kernel";

#[test]
fn links_are_clean_and_the_kernel_reads_the_names_and_literal_targets() {
    let image = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-links.img", std::process::id()),
    );
    let img = image.path().to_str().unwrap();
    let long = "r/".repeat(511) + "x";
    let commands = [
        vec!["/small.txt", "/manyfiles/twin"],
        vec!["-s", "../small.txt", "/sub/relative"],
        vec!["-s", "/small.txt", "/absolute"],
        vec!["-s", &long, "/remote"],
        vec!["/link-short", "/sym-twin"],
    ];
    for args in commands {
        let report = stdout(&ok(tool("fs.xfs").args([img, "ln"]).args(&args)));
        assert_eq!(json_field(&report, "linked"), "true", "{report}");
        assert_eq!(json_field(&report, "path"), *args.last().unwrap());
        assert!(json_field(&report, "inode").parse::<u64>().unwrap() > 0);
        assert_eq!(
            stdout(&ok(tool("fs.xfs").args([img, "get", "dirty", "--text"]))).trim(),
            "false"
        );
    }
    let out = kernel_run(&format!(
        r#"
        set -eu
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        {repair}
        mkdir "$work/m"
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        cd "$work/m"
        test "$(stat -c %i small.txt)" = "$(stat -c %i manyfiles/twin)"
        test "$(stat -c %h small.txt)" = 2
        test "$(stat -c %h manyfiles/twin)" = 2
        cmp small.txt manyfiles/twin
        test "$(readlink sub/relative)" = ../small.txt
        test "$(readlink absolute)" = /small.txt
        test "$(readlink remote)" = {long}
        test "$(stat -c %i link-short)" = "$(stat -c %i sym-twin)"
        test "$(stat -c %h sym-twin)" = 2
        test "$(readlink sym-twin)" = small.txt
        echo KERNEL_LINKS_OK
        cd /
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(img),
        repair = repair::script("\"$work/image\""),
        long = guest_quote(&long),
    ));
    repair::assert_agreed(&out, "the volume fs.xfs ln released, before a kernel mount");
    assert!(out.contains("KERNEL_LINKS_OK"), "{out}");
}

#[test]
fn the_kernel_and_cli_refuse_the_reference_debuggers_maximum_link_count() {
    let image = fixture("xfscli-maxlinks.img");
    let copy =
        scratch::Volume::copy_of(SUITE, &image, &format!("{}-limit.img", std::process::id()));
    let img = copy.path().to_str().unwrap();
    let before = std::fs::read(img).unwrap();
    let out = tool("fs.xfs")
        .args([img, "ln", "/small.txt", "/new"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(out.stdout.is_empty());
    assert!(stderr(&out).contains("2147483647"), "{}", stderr(&out));
    assert!(
        std::fs::read(img).unwrap() == before,
        "refusal changed the image"
    );
    let out = kernel_run(&format!(
        r#"
        set -eu
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        test "$(stat -c %h "$work/m/small.txt")" = 2147483647
        if LC_ALL=C ln "$work/m/small.txt" "$work/m/new" 2>"$work/error"; then
            echo UNEXPECTED_LINK; exit 1
        fi
        grep -q 'Too many links' "$work/error"
        echo KERNEL_LIMIT_OK
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(img),
    ));
    assert!(out.contains("KERNEL_LIMIT_OK"), "{out}");
}
