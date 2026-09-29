//! The multi-call binary: every name answers as itself, the
//! repository-named form reaches the same tool, `--version` identifies
//! the crate, errors are structured, and `doctor` tells our program from
//! whatever else PATH finds under the same name.
//!
//! No fixture, no VM: the unit tier.

mod cli_support;

use cli_support::*;
use std::path::Path;

const CRATE: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[test]
fn the_binary_links_exactly_the_tools_this_crate_can_back() {
    // fs.xfs and nothing else: no mkfs (no initial-layout builder) and no
    // fsck (no checker). A missing link is the packaging-level signal.
    assert_eq!(dotted_names(), vec!["fs.xfs".to_string()]);
}

#[test]
fn every_name_answers_version_with_itself_the_crate_and_the_version() {
    let mut names = dotted_names();
    names.push("rust-fs-xfs".to_string());
    for name in names {
        for flag in ["--version", "-V"] {
            let out = ok(tool(&name).arg(flag));
            assert_eq!(
                stdout(&out).trim_end(),
                format!("{name} ({CRATE}) {VERSION}"),
                "{name} {flag}"
            );
        }
    }
}

#[test]
fn the_repository_name_reaches_a_tool_by_verb_and_by_full_name() {
    let dotted = ok(tool("fs.xfs").arg("--help"));
    for word in ["fs", "fs.xfs"] {
        let repo = ok(tool("rust-fs-xfs").args([word, "--help"]));
        assert_eq!(stdout(&repo), stdout(&dotted), "rust-fs-xfs {word}");
        let version = ok(tool("rust-fs-xfs").args([word, "--version"]));
        assert_eq!(
            stdout(&version).trim_end(),
            format!("fs.xfs ({CRATE}) {VERSION}")
        );
    }
    // cargo's own build, under cargo's name, is the same entry point.
    let cargo = ok(entry().args(["fs", "--help"]));
    assert_eq!(stdout(&cargo), stdout(&dotted));
}

#[test]
fn a_verb_this_crate_does_not_ship_is_a_wrong_command_line() {
    for verb in ["mkfs", "fsck", "mkfs.xfs", "fsck.xfs"] {
        let out = tool("rust-fs-xfs").arg(verb).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "rust-fs-xfs {verb}");
        assert!(
            out.stdout.is_empty(),
            "rust-fs-xfs {verb}: {}",
            stdout(&out)
        );
        assert!(stderr(&out).contains("\"code\": 2"), "{}", stderr(&out));
    }
}

#[test]
fn every_tool_help_carries_an_example_for_every_subcommand() {
    for name in dotted_names() {
        let out = ok(tool(&name).arg("--help"));
        assert!(
            stdout(&out).contains("Examples:"),
            "{name} --help has no example:\n{}",
            stdout(&out)
        );
        let verbs: Vec<String> = stdout(&out)
            .lines()
            .skip_while(|l| !l.starts_with("Commands:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next().map(str::to_string))
            .filter(|v| v != "help")
            .collect();
        assert!(!verbs.is_empty(), "{name} --help lists no subcommands");
        for verb in verbs {
            let sub = ok(tool(&name).args(["x.img", &verb, "--help"]));
            assert!(
                stdout(&sub).contains("Examples:"),
                "{name} {verb} --help has no example:\n{}",
                stdout(&sub)
            );
        }
    }
    let out = ok(tool("rust-fs-xfs").arg("--help"));
    for name in dotted_names() {
        let verb = name.split('.').next().unwrap();
        assert!(
            stdout(&out).contains(&format!("rust-fs-xfs {verb}")),
            "rust-fs-xfs --help does not show `rust-fs-xfs {verb}`:\n{}",
            stdout(&out)
        );
    }
}

#[test]
fn a_wrong_command_line_is_a_structured_error_on_stderr_with_status_2() {
    let out = tool("fs.xfs")
        .args(["x.img", "get", "--no-such-flag"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("{\"error\": \"") && err.trim_end().ends_with("\"code\": 2}"),
        "{err}"
    );
    assert!(err.contains("--no-such-flag"), "{err}");

    // --text: clap's own message, for a person.
    let out = tool("fs.xfs")
        .args(["--text", "x.img", "get", "--no-such-flag"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr(&out).starts_with("error: "), "{}", stderr(&out));
}

#[test]
fn a_failed_run_is_a_structured_error_on_stderr_with_status_1() {
    let missing = scratch_dir("failed-run").join("never-created.img");
    let missing = missing.to_str().unwrap();
    let out = tool("fs.xfs").args([missing, "get"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty(), "stdout: {}", stdout(&out));
    let err = stderr(&out);
    assert!(
        err.starts_with("{\"error\": \"open ") && err.trim_end().ends_with("\"code\": 1}"),
        "{err}"
    );
    let out = tool("fs.xfs")
        .args(["--text", missing, "get"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        stderr(&out).starts_with("fs.xfs: open "),
        "{}",
        stderr(&out)
    );
}

#[test]
fn what_the_library_cannot_do_answers_not_implemented_with_status_3() {
    // Neither verb opens the image: the answer does not depend on it.
    for verb in [&["set", "label", "X"][..], &["resize", "1G"][..]] {
        let out = tool("fs.xfs").arg("x.img").args(verb).output().unwrap();
        assert_eq!(out.status.code(), Some(3), "{verb:?}");
        assert!(out.stdout.is_empty(), "{verb:?}: {}", stdout(&out));
        assert!(
            stderr(&out).starts_with("{\"error\": \"not implemented: "),
            "{verb:?}: {}",
            stderr(&out)
        );
    }
}

// ---------------------------------------------------------------------------
// doctor
// ---------------------------------------------------------------------------

/// A PATH made of `dirs`, and doctor's JSON and status against it.
fn doctor(dirs: &[&Path]) -> (Option<i32>, String) {
    let path = std::env::join_paths(dirs).unwrap();
    let out = entry().arg("doctor").env("PATH", path).output().unwrap();
    (out.status.code(), stdout(&out))
}

/// An executable script at `path` that prints `line` for `--version`.
fn impostor(path: &Path, line: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("#!/bin/sh\necho '{line}'\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn doctor_passes_when_every_name_on_path_is_ours() {
    let (code, json) = doctor(&[names_dir()]);
    assert_eq!(code, Some(0), "{json}");
    assert!(json.contains("\"ok\": true"), "{json}");
    for name in dotted_names() {
        assert!(json.contains(&format!("\"name\": \"{name}\"")), "{json}");
    }
    assert!(!json.contains("\"status\": \"missing\""), "{json}");
}

#[test]
fn doctor_names_a_shadowing_program_and_says_which_path_entry_to_move() {
    let theirs = scratch_dir("foreign");
    impostor(&theirs.join("fs.xfs"), "fs.xfs from somewhere else 1.0");
    let (code, json) = doctor(&[&theirs, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"ok\": false"), "{json}");
    assert!(json.contains("\"status\": \"foreign\""), "{json}");
    assert!(
        json.contains(&format!(
            "\"path\": \"{}\"",
            theirs.join("fs.xfs").display()
        )),
        "{json}"
    );
    assert!(
        json.contains(&format!(
            "put {} before {} on PATH",
            names_dir().display(),
            theirs.display()
        )),
        "{json}"
    );
    // Ours is still found, later, and listed as not run.
    assert!(
        json.contains(&names_dir().join("fs.xfs").display().to_string()),
        "{json}"
    );
}

#[test]
fn doctor_names_the_homebrew_formula_to_unlink() {
    let prefix = scratch_dir("brew");
    let real = prefix.join("Cellar/some-xfs-tools/2.0.0/bin/fs.xfs");
    impostor(&real, "some-xfs-tools 2.0.0");
    let bin_dir = prefix.join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::os::unix::fs::symlink(&real, bin_dir.join("fs.xfs")).unwrap();
    let (code, json) = doctor(&[&bin_dir, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"formula\": \"some-xfs-tools\""), "{json}");
    assert!(json.contains("`brew unlink some-xfs-tools`"), "{json}");
}

#[test]
fn doctor_reports_a_missing_name_with_how_to_install_it() {
    let empty = scratch_dir("empty");
    let (code, json) = doctor(&[&empty]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"missing\""), "{json}");
    assert!(json.contains("chore cli:install"), "{json}");
    assert!(
        json.contains("brew install antimatter-studios/tap/rust-fs-xfs"),
        "{json}"
    );
}

#[test]
fn doctor_reports_our_program_at_another_version_as_stale() {
    let old = scratch_dir("stale");
    impostor(&old.join("fs.xfs"), &format!("fs.xfs ({CRATE}) 0.0.1"));
    let (code, json) = doctor(&[&old, names_dir()]);
    assert_eq!(code, Some(1), "{json}");
    assert!(json.contains("\"status\": \"stale\""), "{json}");
    assert!(
        json.contains(&format!("{CRATE} 0.0.1, not {VERSION}")),
        "{json}"
    );
}

#[test]
fn doctor_text_is_for_a_person_and_keeps_the_fix() {
    let theirs = scratch_dir("text");
    impostor(&theirs.join("fs.xfs"), "something else entirely");
    let path = std::env::join_paths([theirs.as_path(), names_dir()]).unwrap();
    let out = entry()
        .args(["doctor", "--text"])
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = stdout(&out);
    assert!(text.contains("fs.xfs: foreign ("), "{text}");
    assert!(text.contains("  fix: "), "{text}");
    assert!(!text.contains('{'), "{text}");
}
