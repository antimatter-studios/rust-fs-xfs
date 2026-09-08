//! The debug test run guards itself.
//!
//! `ci.yml` runs the unit suite twice: once `--release`, once not. The
//! second run looks redundant and is not. Overflow checks are on in debug
//! and off in release, so a defect whose only symptom is an arithmetic
//! overflow panic cannot be observed by a release-only gate — which is
//! what this crate had, in every workflow it has: `ci.yml` was `--release`
//! at both invocations, `scripts/ci-test.sh` fixes `--release` for the
//! whole suite matrix, and `release.yml` runs no `cargo test` at all.
//!
//! The comment above that step explains the reasoning, but a comment is
//! advice. Deleting the step leaves CI green, saves a compile, and puts
//! the blindness back. The failure mode is a well-intentioned tidy-up:
//! nobody removes a test job on purpose, they consolidate two lines that
//! appear to do the same thing.
//!
//! So the property gets a check of its own. It asserts that at least one
//! `cargo test` in the gate still runs without `--release`, rather than
//! that any particular line is present, so renaming or reformatting the
//! step does not defeat it while a deletion does.
//!
//! # Why this is an integration test and not a module under `src/`
//!
//! Cargo discovers `tests/*.rs` on its own, so there is no declaration
//! anywhere that can be deleted to switch this off. The first draft was
//! `src/ci_profile.rs` behind a `#[cfg(test)] mod ci_profile;` in
//! `lib.rs`, and while writing it a stray `git reset --hard` removed that
//! one line: the file stayed, `cargo test` went green, and seven
//! assertions had quietly ceased to exist. Nothing warns about an
//! unreferenced file under `src/` — it is not compiled, so there is no
//! dead-code lint to fire, and `cargo fmt --check` does not look at it
//! either.
//!
//! That is the same defect as the one this file exists to prevent, one
//! level up: a check that is present, that nothing runs. Being here
//! makes it structural rather than remembered.
//!
//! It therefore runs in the `--release` step at `ci.yml:62` rather than
//! in the debug step it protects. That is fine and deliberate: what it
//! reads is a text file, so the profile it runs under is irrelevant to
//! its result, and being in the first test step of the gate means a
//! deleted debug step fails early.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Read a file the guards depend on, or fail.
///
/// It panics rather than returning `None` on purpose. An
/// `if !path.exists() { return }` anywhere in this module would
/// reproduce the exact class of blindness the module exists to prevent:
/// an assertion that is present, runs, and cannot report the thing it
/// was written for. A missing workflow is a finding, not a skip.
fn read_or_panic(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. This guard must fail rather than skip: a \
             version of it that returned early here would be the same \
             blindness it exists to prevent.",
            path.display()
        )
    })
}

/// Every `cargo test` invocation in a workflow that would be compiled
/// with overflow checks on.
///
/// Four things disqualify a line, and each one is a way the guard could
/// otherwise be satisfied by something that does not actually build in
/// debug:
///
/// - it is a YAML comment. This is not defensive here, it is load
///   bearing: `ci.yml` quotes `cargo test --locked --lib` verbatim
///   inside the comment block that explains the step, so a scan that
///   ignored comments would still find it after the step itself had
///   been deleted, and would pass;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly;
/// - it sets a `CARGO_PROFILE_*` variable, which can turn overflow
///   checks off for the dev profile from outside the manifest.
///
/// A line invoking `scripts/ci-test.sh` is not counted either, and needs
/// no rule of its own: the script supplies `cargo test --release`
/// itself, so no literal `cargo test` appears in the workflow. That
/// premise is asserted by [`ci_test_sh_still_supplies_release_itself`]
/// rather than assumed.
fn runs_with_overflow_checks(workflow: &str) -> Vec<String> {
    workflow
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            if !command.contains("cargo test") {
                return None;
            }
            if command.contains("--release")
                || command.contains("--profile")
                || command.contains("CARGO_PROFILE_")
            {
                return None;
            }
            Some(command.to_string())
        })
        .collect()
}

/// The guard. Reads the workflow this repository's pull requests are
/// gated by and refuses if nothing in it compiles the overflow checks.
///
/// `ci.yml` specifically, not every workflow. `stress.yml` is scheduled
/// and `release.yml` runs no tests, so a debug run in either would
/// satisfy a broader scan without covering a single pull request.
#[test]
fn the_gate_still_tests_in_a_profile_that_can_see_an_overflow() {
    let path = manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let workflow = read_or_panic(&path);

    let debug_runs = runs_with_overflow_checks(&workflow);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` in {} runs without `--release`, so no defect whose \
         only symptom is an arithmetic overflow panic can be observed by \
         this repository's gate. Overflow checks are on in debug and off in \
         release. If the debug step looked redundant beside the release one, \
         it is not — see the comment above it.",
        path.display()
    );
}

/// The debug runs that ask the build to prove it traps an overflow.
///
/// A subset of [`runs_with_overflow_checks`]: those which also set the
/// `EXPECT_OVERFLOW_CHECKS` handshake, so that
/// `overflow_checks::the_build_the_gate_asked_to_check_does_check`
/// performs an overflow and fails if the build let it through.
///
/// A run carrying the handshake but also `--release` is not counted,
/// because `runs_with_overflow_checks` has already excluded it. Such a
/// step is a misconfiguration and it fails loudly rather than quietly:
/// the checks are legitimately off in release, so the assertion the
/// handshake arms would fire there every time.
fn debug_runs_that_prove_the_build_traps(workflow: &str) -> Vec<String> {
    runs_with_overflow_checks(workflow)
        .into_iter()
        .filter(|command| command.contains("EXPECT_OVERFLOW_CHECKS=1"))
        .collect()
}

/// The half a text scan cannot do, delegated to the build itself.
///
/// # Why this exists rather than more spellings
///
/// The manifest scan below reads `Cargo.toml` and asks whether a known
/// spelling of "overflow checks are off" is present. Four spellings of
/// the key were needed before it was right. Then two more routes turned
/// up that are not in that file at all: a
/// `CARGO_PROFILE_TEST_OVERFLOW_CHECKS` variable set at step or job
/// level in the workflow, which the parser cannot see because it only
/// reads `run:` lines, and a `.cargo/config.toml`, which nothing here
/// reads. Both leave the debug step present, running, green and blind.
///
/// All six are the same shape: a scanner enumerating the ways a thing
/// can be disabled, in the places it happens to look. Another pass buys
/// the next one. So the question is put to the build instead -- perform
/// an overflow, see whether you are stopped -- and this test's job
/// shrinks to making sure the gate still asks it.
#[test]
fn the_debug_run_asks_the_build_to_prove_it_traps_overflows() {
    let path = manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml");
    let workflow = read_or_panic(&path);

    let proving = debug_runs_that_prove_the_build_traps(&workflow);
    assert!(
        !proving.is_empty(),
        "no `cargo test` in {} runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1, so nothing checks whether the profile \
         the gate builds actually traps an arithmetic overflow. Reading \
         Cargo.toml is not enough: the checks can also be turned off by a \
         CARGO_PROFILE_TEST_OVERFLOW_CHECKS variable at step or job level, \
         or by a .cargo/config.toml, neither of which is in any file this \
         test reads. The handshake is what arms the one check that cannot \
         be fooled by where the setting lives.",
        path.display()
    );
}

/// The premise behind not counting the `scripts/ci-test.sh` steps.
///
/// Most suites here run through that script, which supplies `--release`
/// itself, so those workflow lines carry no literal `cargo test` and the
/// parser does not see them. That is correct today and is the whole
/// reason the parser needs no rule about them. If the script ever took
/// the profile as an argument, the exclusion would begin hiding a real
/// debug run: the guard would go red while the property it checks was
/// satisfied. Red is the safe direction, but a stale premise is worth
/// naming here rather than discovering from a confusing failure.
#[test]
fn ci_test_sh_still_supplies_release_itself() {
    let path = manifest_dir().join("scripts").join("ci-test.sh");
    let script = read_or_panic(&path);

    let invocations: Vec<&str> = script
        .lines()
        .map(str::trim_start)
        .filter(|line| !line.starts_with('#') && line.contains("cargo test"))
        .collect();

    // Non-emptiness first. `all()` over nothing is true, and a rewritten
    // script that no longer invokes cargo would satisfy the assertion
    // below while establishing nothing at all.
    assert!(
        !invocations.is_empty(),
        "{} no longer invokes `cargo test`, so the reason the workflow's \
         ci-test.sh steps are not counted as debug runs no longer holds. \
         Re-read `runs_with_overflow_checks` before changing either.",
        path.display()
    );

    for line in &invocations {
        assert!(
            line.contains("--release"),
            "{}: `{line}` no longer pins `--release`. The parser does not \
             count the workflow's ci-test.sh steps because the script chose \
             the profile; if the caller can choose it now, those steps may \
             be real debug runs and the parser must learn to read them.",
            path.display()
        );
    }
}

/// The profiles that switch overflow checks off for the run `cargo test`
/// builds, named by section.
///
/// Only `profile.dev` and `profile.test` count. `cargo test` uses the
/// `test` profile, which inherits from `dev`, so either can disable the
/// checks in one line. `profile.release` is deliberately not included:
/// the checks are off there by default, that is what ships, and the
/// release step exists to test what ships.
/// The full dotted paths that switch overflow checks off for the profile
/// `cargo test` builds.
///
/// # This compares a whole path, because a key is not a word
///
/// The first version tracked the `[section]` and compared the key to the
/// literal `"overflow-checks"`. That reads correctly and is defeated by
/// ordinary TOML, because the same setting has several spellings and
/// cargo honours all of them without a warning. Measured on this
/// branch, with a runtime `u64::MAX + 1` unit test as the probe --
/// `cargo test --locked --lib` EXIT=101 means the checks are on, EXIT=0
/// means they are off, and `cargo metadata --no-deps` was EXIT=0 for
/// every one:
///
/// ```text
///   (nothing)                                          EXIT=101  on
///   [profile.test]  overflow-checks = false            EXIT=0    off
///   [profile.test]  "overflow-checks" = false          EXIT=0    off
///   [profile.test]  'overflow-checks' = false          EXIT=0    off
///   [profile]       test.overflow-checks = false       EXIT=0    off
/// ```
///
/// A bare key, a basic string, a literal string, and a dotted key that
/// puts the profile name on the key side where a section-matching scan
/// never looks. Three of those four defeated the first version, and each
/// leaves the debug step in `ci.yml` present, running, green and blind
/// -- the exact state the guard exists to refuse.
///
/// So the section and the key are joined into one path and normalised
/// per segment, and the comparison is against the whole thing. That
/// covers the spellings above, a quoted *section* (`["profile"."test"]`),
/// and a fully top-level dotted key with no section at all.
///
/// `profile.release` is deliberately absent: the checks are off there by
/// default, that is what ships, and the release step exists to test what
/// ships.
fn profiles_disabling_overflow_checks(manifest: &str) -> Vec<String> {
    /// Split a dotted TOML path and strip each segment's quoting, so
    /// that `"profile" . 'test'` and `profile.test` are one path.
    fn normalise(path: &str) -> String {
        path.split('.')
            .map(|segment| {
                segment
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .trim()
            })
            .collect::<Vec<_>>()
            .join(".")
    }

    const DISABLED: [&str; 2] = [
        "profile.dev.overflow-checks",
        "profile.test.overflow-checks",
    ];

    let mut section = String::new();
    let mut found = Vec::new();
    for raw in manifest.lines() {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.starts_with('[') {
            section = normalise(line.trim_matches(|c| c == '[' || c == ']'));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if value.trim() != "false" {
            continue;
        }
        let key = normalise(key);
        let path = if section.is_empty() {
            key
        } else {
            format!("{section}.{key}")
        };
        if DISABLED.contains(&path.as_str()) {
            found.push(path);
        }
    }
    found
}

/// The other half of the same property, and the one the guard above
/// cannot see.
///
/// A debug step in `ci.yml` only buys anything while the profile it
/// builds actually checks. One line — `overflow-checks = false` under
/// `[profile.test]`, a plausible way to make a slow suite faster —
/// would leave that step present, running, green, and no longer able to
/// observe an overflow, with every assertion above still passing. A
/// guard for half a condition is the defect it was written to prevent.
#[test]
fn the_profile_that_cargo_test_builds_still_checks_for_overflow() {
    let path = manifest_dir().join("Cargo.toml");
    let manifest = read_or_panic(&path);

    let disabled = profiles_disabling_overflow_checks(&manifest);
    assert!(
        disabled.is_empty(),
        "{} sets `overflow-checks = false` under {disabled:?}. `cargo test` \
         builds the `test` profile, which inherits from `dev`, so this \
         switches off the check that the debug step in ci.yml exists to \
         run — leaving that step present, green, and blind. Put it back, \
         or the debug step is costing a compile and buying nothing.",
        path.display()
    );
}

/// The parser is the part of this that can rot, so it is checked against
/// each shape it has to tell apart.
mod parser {
    use super::runs_with_overflow_checks;

    /// The trap this repository actually contains. `ci.yml` documents the
    /// debug step by quoting the command, so the text survives the step's
    /// deletion.
    #[test]
    fn a_debug_run_quoted_in_a_comment_does_not_count() {
        let quoted_in_a_comment = "\
jobs:
  test:
    steps:
      # Measured on this branch:
      #     cargo test --locked --release --lib   ->  EXIT=0
      #     cargo test --locked --lib             ->  EXIT=101
      - run: cargo test --locked --release
";
        assert_eq!(
            runs_with_overflow_checks(quoted_in_a_comment),
            Vec::<String>::new(),
            "a debug command quoted inside a comment is documentation, not a run"
        );
    }

    #[test]
    fn a_real_debug_run_counts() {
        let with_the_step = "\
jobs:
  test:
    steps:
      - run: cargo test --locked --release
      - run: cargo test --locked --lib
";
        assert_eq!(
            runs_with_overflow_checks(with_the_step),
            vec!["- run: cargo test --locked --lib".to_string()],
        );
    }

    /// A step whose command is `--release` but which carries a trailing
    /// comment mentioning the debug run.
    #[test]
    fn a_trailing_comment_does_not_promote_a_release_run() {
        let inline = "      - run: cargo test --locked --release  # not cargo test --lib\n";
        assert_eq!(
            runs_with_overflow_checks(inline),
            Vec::<String>::new(),
            "the command is --release; the comment after it is not a second run"
        );
    }

    /// The inline-comment strip, which nothing else here pins. A real
    /// debug run whose trailing comment happens to contain `--release`
    /// must still be counted. Without the strip that word disqualifies
    /// the command, and the guard then fails insisting there is no debug
    /// run while one is sitting in front of it.
    #[test]
    fn a_trailing_comment_naming_release_does_not_disqualify_a_debug_run() {
        let line = "      - run: cargo test --locked --lib  # deliberately not --release\n";
        assert_eq!(
            runs_with_overflow_checks(line),
            vec!["- run: cargo test --locked --lib".to_string()],
            "the command is a debug run; --release appears only in its comment"
        );
    }

    /// The ways a run can carry no `--release` and still be built without
    /// the checks.
    #[test]
    fn a_profile_named_another_way_does_not_count() {
        let lines = [
            "      - run: cargo test --locked --profile release-with-debug --lib",
            "      - run: CARGO_PROFILE_TEST_OVERFLOW_CHECKS=false cargo test --locked --lib",
        ];
        for line in lines {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} does not compile the overflow checks"
            );
        }
        assert_eq!(
            lines.len(),
            2,
            "the loop above must have examined both shapes"
        );
    }

    /// The `ci-test.sh` steps, which are how most suites in this
    /// repository run. They are not debug runs: the script adds
    /// `--release`. Nothing in the parser says so — they simply carry no
    /// `cargo test` — and this pins that they stay uncounted, so a later
    /// "helpful" widening of the pattern to `cargo|ci-test` cannot start
    /// reporting a release run as a debug one.
    #[test]
    fn a_ci_test_sh_step_is_not_a_debug_run() {
        let via_the_script = "\
jobs:
  oracle:
    steps:
      - run: ./scripts/ci-test.sh --test dir_block_oracle
      - run: ./scripts/ci-test.sh --test \"$suite\"
";
        assert_eq!(
            runs_with_overflow_checks(via_the_script),
            Vec::<String>::new(),
            "ci-test.sh supplies --release itself, so these are release runs"
        );
    }
}

/// The manifest scanner, held to the shapes it has to tell apart. These
/// do not depend on this repository's own `Cargo.toml`, so they keep
/// meaning something after it changes.
mod manifest_parser {
    use super::profiles_disabling_overflow_checks;

    #[test]
    fn the_test_profile_disabling_the_checks_is_caught() {
        let manifest = "\
[profile.release]
lto = true

[profile.test]
overflow-checks = false
";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn the_dev_profile_disabling_the_checks_is_caught() {
        let manifest = "[profile.dev]\noverflow-checks   =   false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// Release is expected to have them off. Flagging it would make the
    /// guard fail on every correct manifest, which is the fastest way to
    /// get a guard deleted.
    #[test]
    fn the_release_profile_disabling_the_checks_is_not_flagged() {
        let manifest = "[profile.release]\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// A commented-out line is not a setting -- the same trap as the
    /// workflow parser's, in the other file this module reads.
    #[test]
    fn a_commented_out_setting_is_not_a_setting() {
        let manifest = "[profile.test]\n# overflow-checks = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// The comment strip, which nothing else here pins. The realistic
    /// way this setting arrives is with its excuse on the same line, and
    /// it must still be caught: unstripped, the value reads
    /// `false  # speeds the suite up`, which is not `false`, and the
    /// guard waves through the exact edit it exists to catch.
    #[test]
    fn a_disabling_line_with_a_trailing_comment_is_still_caught() {
        let manifest = "[profile.test]\noverflow-checks = false  # speeds the suite up\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// A different setting being `false` is not this setting being
    /// `false`. Without this the scanner could be keying on the value
    /// alone -- flagging any `= false` under those two sections -- and
    /// every other test here would still pass.
    #[test]
    fn another_setting_being_false_is_not_this_one() {
        let manifest = "[profile.test]\ndebug-assertions = false\nopt-level = 1\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// THE SPELLINGS THAT DEFEATED THE FIRST VERSION. Each of these was
    /// measured to genuinely switch the checks off, with no warning from
    /// cargo -- see the table on `profiles_disabling_overflow_checks`.
    /// A guard that reads one spelling of a setting is a guard against
    /// typing it one way.
    #[test]
    fn a_double_quoted_key_is_the_same_key() {
        let manifest = "[profile.test]\n\"overflow-checks\" = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_literal_quoted_key_is_the_same_key() {
        let manifest = "[profile.dev]\n'overflow-checks' = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.dev.overflow-checks".to_string()],
        );
    }

    /// The one a section-matching scan cannot see at all: the profile
    /// name is on the key side, so the section is only `profile`.
    #[test]
    fn a_dotted_key_putting_the_profile_on_the_key_side_is_caught() {
        let manifest = "[profile]\ntest.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// And with no section header at all, which is still valid TOML.
    #[test]
    fn a_top_level_dotted_key_is_caught() {
        let manifest = "profile.test.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    #[test]
    fn a_quoted_section_is_the_same_section() {
        let manifest = "[\"profile\".'test']\noverflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            vec!["profile.test.overflow-checks".to_string()],
        );
    }

    /// Release stays exempt in the dotted spelling too, or normalising
    /// the path would have quietly widened what the guard refuses.
    #[test]
    fn the_release_profile_is_exempt_in_the_dotted_spelling_too() {
        let manifest = "[profile]\nrelease.overflow-checks = false\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }

    /// `true` is the state we want and must not be reported as the state
    /// we do not. Without this the scanner could be keying on the word
    /// `overflow-checks` alone and nothing here would notice.
    #[test]
    fn enabling_the_checks_explicitly_is_not_flagged() {
        let manifest = "[profile.test]\noverflow-checks = true\n";
        assert_eq!(
            profiles_disabling_overflow_checks(manifest),
            Vec::<String>::new(),
        );
    }
}

/// The handshake half of the workflow parser.
mod handshake {
    use super::debug_runs_that_prove_the_build_traps;

    #[test]
    fn a_debug_run_carrying_the_handshake_counts() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            vec!["- run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib".to_string()],
        );
    }

    /// The state this branch shipped in before: a debug run that exists
    /// and asks the build nothing.
    #[test]
    fn a_debug_run_without_the_handshake_does_not_count() {
        let yaml = "      - run: cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
            "the step is there but nothing checks the build it produced"
        );
    }

    /// A handshake on a release run proves nothing and must not satisfy
    /// this: the checks are off in release on purpose.
    #[test]
    fn the_handshake_on_a_release_run_does_not_count() {
        let yaml = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --release\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
        );
    }

    /// And quoted inside the comment block that explains it, which is
    /// where ci.yml also mentions it.
    #[test]
    fn the_handshake_quoted_in_a_comment_does_not_count() {
        let yaml = "      #     EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";
        assert_eq!(
            debug_runs_that_prove_the_build_traps(yaml),
            Vec::<String>::new(),
        );
    }
}
