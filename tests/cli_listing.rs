mod cli_support;

use cli_support::*;
use std::process::Output;

fn listing_paths(output: &Output) -> Vec<String> {
    stdout(output)
        .split("\n  {")
        .skip(1)
        .map(|row| json_field(row, "path"))
        .collect()
}

#[test]
fn recursive_listing_help_uses_the_shared_arguments_and_defaults() {
    let output = ok(tool("fs.xfs").args(["disk.img", "ls", "--help"]));
    let help = stdout(&output)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    for expected in [
        "Walk depth first, names sorted as bytes; never follow a symlink",
        "How deep --recursive may go (default: 64); a directory's entries are depth 1",
        "How many entries --recursive may list (default: 100000)",
        "-R, --recursive",
        "--max-depth <N>",
        "--limit <N>",
        "(default: 64)",
        "(default: 100000)",
    ] {
        assert!(help.contains(expected), "missing {expected:?}:\n{help}");
    }
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
}

#[test]
fn recursive_listing_bounds_require_recursion_and_unsigned_values() {
    for arguments in [
        vec!["--max-depth", "1"],
        vec!["--limit", "1"],
        vec!["-R", "--max-depth", "-1"],
        vec!["--recursive", "--limit", "nope"],
    ] {
        let output = tool("fs.xfs")
            .args(["disk.img", "ls"])
            .args(&arguments)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{arguments:?}");
        assert!(output.stdout.is_empty(), "{}", stdout(&output));
        assert_eq!(json_field(&stderr(&output), "code"), "2");
    }
}

#[test]
fn recursive_listing_reached_bounds_keep_the_prefix_and_exact_fits_succeed() {
    let scratch = scratch_dir("listing-bounds");
    let image = scratch.path().join("listing.img");
    let image = image.to_str().unwrap();
    ok(tool("mkfs.xfs").args(["--size", "320M", image]));
    ok(tool("fs.xfs").args([image, "mkdir", "--parents", "/a/child"]));
    ok(tool("fs.xfs").args([image, "mkdir", "/z"]));
    let written = fs_write(image, "/z/file", b"payload");
    assert!(written.status.success(), "{}", stderr(&written));

    let complete = ok(tool("fs.xfs").args([image, "ls", "--recursive"]));
    let expected = vec!["/a", "/a/child", "/z", "/z/file"];
    assert_eq!(listing_paths(&complete), expected);
    let exact = ok(tool("fs.xfs").args([image, "ls", "-R", "--max-depth", "2", "--limit", "4"]));
    assert_eq!(exact.stdout, complete.stdout);

    for (bound, value, count, stopped) in [
        ("--max-depth", "0", 0, "/a"),
        ("--max-depth", "1", 1, "/a/child"),
        ("--limit", "0", 0, "/a"),
        ("--limit", "1", 1, "/a/child"),
        ("--limit", "3", 3, "/z/file"),
    ] {
        for recursive in ["--recursive", "-R"] {
            let output = tool("fs.xfs")
                .args([image, "ls", recursive, bound, value])
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(1), "{bound} {value}");
            assert_eq!(listing_paths(&output), expected[..count]);
            assert_eq!(
                json_field(&stderr(&output), "error"),
                format!("{bound} {value} reached at {stopped}")
            );
            assert_eq!(json_field(&stderr(&output), "code"), "1");
        }
    }

    let zero_file = tool("fs.xfs")
        .args([image, "ls", "-R", "--limit", "0", "/z/file"])
        .output()
        .unwrap();
    assert_eq!(zero_file.status.code(), Some(1));
    assert_eq!(stdout(&zero_file).trim(), "[]");
    assert_eq!(
        json_field(&stderr(&zero_file), "error"),
        "--limit 0 reached at /z/file"
    );
    let file = ok(tool("fs.xfs").args([
        image,
        "ls",
        "-R",
        "--max-depth",
        "0",
        "--limit",
        "1",
        "/z/file",
    ]));
    assert_eq!(listing_paths(&file), ["/z/file"]);
    assert_eq!(json_field(&stdout(&file), "depth"), "0");
    let empty = ok(tool("fs.xfs").args([
        image,
        "ls",
        "-R",
        "--max-depth",
        "0",
        "--limit",
        "0",
        "/a/child",
    ]));
    assert_eq!(stdout(&empty).trim(), "[]");
}
