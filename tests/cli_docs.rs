//! The man pages and shell completions the binary writes for packaging
//! (`rust-fs-xfs generate man|completions SHARE`): one page per name in
//! its section, each tool's page naming every one of its subcommands, and
//! a zsh, bash and fish completion per name.

mod cli_support;

use cli_support::*;

fn generated(tag: &str) -> Scratch {
    let scratch = scratch_dir(&format!("docs-{tag}"));
    let share_arg = scratch.path().to_string_lossy().into_owned();
    ok(entry().args(["generate", "man", &share_arg]));
    ok(entry().args(["generate", "completions", &share_arg]));
    scratch
}

fn section(name: &str) -> u8 {
    if name.starts_with("mkfs.") || name.starts_with("fsck.") {
        8
    } else {
        1
    }
}

#[test]
fn every_name_has_a_man_page_in_its_section_naming_its_subcommands() {
    let scratch = generated("man");
    let share = scratch.path();
    let mut names = dotted_names();
    names.push("rust-fs-xfs".to_string());
    for name in &names {
        let s = section(name);
        let page = share.join(format!("man/man{s}/{name}.{s}"));
        let text =
            std::fs::read_to_string(&page).unwrap_or_else(|e| panic!("{}: {e}", page.display()));
        assert!(text.contains(".TH "), "{name}: not a man page");
        assert!(text.contains("Examples:"), "{name}: no example");
        // Every subcommand --help lists is named on the page.
        let help = stdout(&ok(tool(name).arg("--help")));
        let verbs: Vec<&str> = help
            .lines()
            .skip_while(|l| !l.starts_with("Commands:"))
            .skip(1)
            .take_while(|l| l.starts_with("  "))
            .filter_map(|l| l.split_whitespace().next())
            .filter(|v| *v != "help")
            .collect();
        for verb in verbs {
            assert!(text.contains(verb), "{name}'s page does not mention {verb}");
        }
    }
    // Section 8 holds the two system-administration tools (#339).
    for page in ["mkfs.xfs.8", "fsck.xfs.8"] {
        assert!(share.join("man/man8").join(page).is_file(), "no {page}");
    }
    // fs.xfs's subcommands have pages of their own, which its list names.
    for verb in [
        "ls", "stat", "read", "write", "mkdir", "rm", "rmdir", "get", "info", "set", "resize",
    ] {
        assert!(
            share.join(format!("man/man1/fs.xfs-{verb}.1")).is_file(),
            "no fs.xfs-{verb}.1"
        );
    }
    let ls = std::fs::read_to_string(share.join("man/man1/fs.xfs-ls.1")).unwrap();
    for option in ["recursive", "max-depth", "limit"] {
        assert!(ls.contains(option), "ls man page omits {option}");
    }
}

#[test]
fn fsck_help_and_manual_describe_repair_writes_and_every_exit_status() {
    let scratch = generated("fsck");
    let manual = std::fs::read_to_string(scratch.path().join("man/man8/fsck.xfs.8"))
        .expect("read fsck.xfs(8)")
        .replace("\\-", "-");
    for (name, text) in [
        (
            "fsck.xfs --help",
            stdout(&ok(tool("fsck.xfs").arg("--help"))),
        ),
        (
            "rust-fs-xfs fsck --help",
            stdout(&ok(entry().args(["fsck", "--help"]))),
        ),
        ("fsck.xfs(8)", manual),
    ] {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        for promise in [
            "By default and with -n, nothing is written",
            "--dry-run plans a repair",
            "-y and -p apply the repair plan and write changes",
        ] {
            assert!(text.contains(promise), "{name} omits {promise:?}: {text}");
        }
        assert!(
            !text.contains("Nothing is written, and nothing is repaired"),
            "{name} still promises check-only behaviour in every mode"
        );
        let (_, statuses) = text
            .split_once("Exit status")
            .expect("exit status contract");
        // fsck(8)'s bitmask: 5 combines corrected (1) and uncorrected (4).
        for status in [
            "0 clean",
            "1 errors corrected",
            "4 errors left uncorrected",
            "5 errors corrected and others left uncorrected",
            "8 the target could not be checked",
            "16 a wrong command line",
        ] {
            assert!(
                statuses.contains(status),
                "{name} omits {status:?}: {statuses}"
            );
        }
    }
}

#[test]
fn the_fsck_tool_description_discloses_repair_writes() {
    let help = stdout(&ok(entry().arg("--help")));
    let description = help
        .lines()
        .find(|line| line.split_whitespace().next() == Some("fsck"))
        .expect("the entry point lists fsck");
    assert!(
        description.contains("-y and -p write repairs"),
        "the fsck tool description hides its write modes: {description}"
    );
}

#[test]
fn every_name_has_a_zsh_bash_and_fish_completion() {
    let scratch = generated("completions");
    let share = scratch.path();
    let mut names = dotted_names();
    names.push("rust-fs-xfs".to_string());
    for name in &names {
        let zsh = std::fs::read_to_string(share.join(format!("zsh/site-functions/_{name}")))
            .unwrap_or_else(|e| panic!("zsh {name}: {e}"));
        assert!(
            zsh.starts_with(&format!("#compdef {name}")),
            "zsh {name}: {}",
            &zsh[..40.min(zsh.len())]
        );
        let bash =
            std::fs::read_to_string(share.join(format!("bash-completion/completions/{name}")))
                .unwrap_or_else(|e| panic!("bash {name}: {e}"));
        assert!(bash.contains("complete -F"), "bash {name}");
        let fish =
            std::fs::read_to_string(share.join(format!("fish/vendor_completions.d/{name}.fish")))
                .unwrap_or_else(|e| panic!("fish {name}: {e}"));
        assert!(fish.contains(&format!("complete -c {name}")), "fish {name}");
        if name == "fs.xfs" {
            for completion in [&zsh, &bash, &fish] {
                for option in ["recursive", "max-depth", "limit"] {
                    assert!(completion.contains(option), "completion omits {option}");
                }
            }
        }
    }
}
