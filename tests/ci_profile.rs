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

use saphyr::{LoadableYamlNode, Yaml};
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
fn runs_with_overflow_checks(script: &str) -> Vec<String> {
    script
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

/// A workflow, structured just far enough to answer one question:
/// does this step's result actually gate a pull request?
///
/// The line-based scan above finds the command. It cannot see the
/// step's sibling keys, so `if: false` and `continue-on-error: true`
/// left every guard test green while the gate stopped gating -- a step
/// that runs and whose result nothing reads, which is this project's
/// own named defect, committed inside the guard written to prevent it.
///
/// The conditions are ENUMERATED rather than patched one defeat at a
/// time, because twice on this shape the defeat lived in what the scan
/// does not look at rather than in what it compares. A `run:` step
/// gates a pull request only if the step carries no `if:` and no
/// `continue-on-error:`, its job carries neither either, and the
/// workflow still triggers on `pull_request`.
///
/// `if:` and `continue-on-error:` are rejected on the KEY'S PRESENCE,
/// not by evaluating it. `if: false`, `if: ${{ false }}` and an `if:`
/// on an expression that happens to be false are distinct spellings,
/// and four spellings of one manifest key had already defeated a
/// matcher on a sibling repository -- enumerating them is the losing
/// game. Over-strict is the safe direction: a step that genuinely
/// needs a condition can be split out, whereas a guard that
/// interprets conditions acquires a new defeat whenever the syntax
/// grows.
///
/// A step's other keys -- `name:`, `env:`, `uses:`/`with:` -- say
/// nothing about whether the result is read, so they are ACCEPTED. An
/// `env:` mapping in particular must not disqualify a step: that would
/// be over-strictness in the one direction that costs something, since
/// the handshake this guard looks for is itself an environment
/// variable and a maintainer may reasonably move it into a mapping.
///
/// # WHY THIS IS PARSED AND NO LONGER SCANNED
///
/// The version this replaces hand-rolled the YAML, and it was correct
/// only in the sense that it had been patched five times. Each patch
/// was a helper taught one more piece of ordinary grammar:
///
/// ```text
///   without_comment        a trailing `#`, so a commented-out trigger
///                          stopped counting as a trigger
///   key_of                 quotes, so `"if": false` stopped being a
///                          different key from `if: false`
///   opens_a_block_scalar   `|-`, `|+`, `>`, `>-`, `>+`, `|2`, `>2-`,
///                          so a block's contents were read at all
///   indent_of              the block structure itself
///   triggers: Vec<String>  whole names, so `pull_request_review` and
///                          `pull_request` stopped being the same
/// ```
///
/// Every one of those is a rule a YAML parser already has. And the
/// cost of learning them by hand is recorded in this file, twice over:
/// `key_of`'s own comment noted that the identical quote-normalisation
/// had already been added to `profiles_disabling_overflow_checks` a
/// few dozen lines above, after a quoted `"overflow-checks" = false`
/// defeated that scan -- the lesson did not travel between two parsers
/// in one file. Learning it a sixth time was the alternative to this.
///
/// The properties those helpers defended are not dropped with them.
/// Each is now asserted in `mod gating` against the parser instead:
/// every block scalar style is read whole, a quoted key is the same
/// key, a commented-out trigger is not a trigger, and a `#` inside a
/// quoted shell value is content rather than a comment.
///
/// `saphyr` is a dev-dependency, so nothing here reaches a consumer of
/// the crate.
#[derive(Debug)]
struct Step {
    keys: Vec<String>,
    run: String,
}

#[derive(Debug)]
struct Job {
    keys: Vec<String>,
    steps: Vec<Step>,
}

#[derive(Debug)]
struct Workflow {
    /// The trigger NAMES, parsed. Not the `on:` block's text: a
    /// substring search over that text answered `true` for
    /// `pull_request_review:` and for `pull_request` sitting inside a
    /// comment, so the guard reported pull-request coverage that was
    /// not there. Neither spelling needs an adversarial author.
    triggers: Vec<String>,
    jobs: Vec<Job>,
}

/// The value of `name` in a YAML mapping, or `None`.
///
/// By name rather than by constructing a key, because `saphyr`'s `Yaml`
/// borrows the source text and building one to hand to `get` is more
/// ceremony than the lookup is worth here.
fn field<'a, 'b>(node: &'a Yaml<'b>, name: &str) -> Option<&'a Yaml<'b>> {
    node.as_mapping()?
        .iter()
        .find(|(key, _)| key.as_str() == Some(name))
        .map(|(_, value)| value)
}

/// The keys of a YAML mapping, as plain strings.
///
/// The parser has already resolved the quoting, so `"if"`, `'if'` and
/// `if` all arrive here as `if`. That is the whole of what `key_of`
/// did: there is no un-quoting step left to forget.
fn keys_of(node: &Yaml) -> Vec<String> {
    node.as_mapping()
        .map(|mapping| {
            mapping
                .iter()
                .filter_map(|(key, _)| key.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Structure a workflow far enough to answer the questions above.
///
/// Panics on a workflow it cannot parse, deliberately. A guard that
/// returned an empty `Workflow` for a file it did not understand would
/// report "no debug run gates this" -- a failure, so that direction is
/// safe -- but one that returned early with a PASS would be the
/// blindness this module exists to prevent. Failing on the parse error
/// names the real problem instead of a consequence of it.
fn parse_workflow(text: &str) -> Workflow {
    let documents = Yaml::load_from_str(text).unwrap_or_else(|e| {
        panic!(
            "workflow is not valid YAML: {e}. This guard reads the workflow \
             rather than scanning its text, so a file it cannot parse is a \
             failure and never a pass."
        )
    });
    let Some(document) = documents.first() else {
        return Workflow {
            triggers: Vec::new(),
            jobs: Vec::new(),
        };
    };

    // `on:` takes three legal shapes: a mapping of trigger names, a
    // sequence of them, or a single scalar. All three are names.
    //
    // Note that `on` survives as the string key `on` and is not folded
    // into the boolean `true` -- saphyr implements the YAML 1.2 core
    // schema, where only `true`/`false` are booleans. The YAML 1.1
    // reading that would break every GitHub workflow ever written does
    // not apply.
    let triggers = match field(document, "on") {
        Some(on) if on.as_mapping().is_some() => keys_of(on),
        Some(on) if on.as_sequence().is_some() => on
            .as_sequence()
            .into_iter()
            .flatten()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(on) => on.as_str().map(str::to_string).into_iter().collect(),
        None => Vec::new(),
    };

    let mut jobs = Vec::new();
    if let Some(mapping) = field(document, "jobs").and_then(Yaml::as_mapping) {
        for (_, body) in mapping.iter() {
            let steps = field(body, "steps")
                .and_then(Yaml::as_sequence)
                .into_iter()
                .flatten()
                .map(|step| Step {
                    keys: keys_of(step),
                    // A `run:` block of any style -- `|`, `|-`, `|+`,
                    // `>`, `>-`, `>+`, `|2`, `>2-` -- arrives as one
                    // string with the block folded per its own rules,
                    // so a command inside a shell loop is seen whole
                    // rather than as fragments, and no style is
                    // mistaken for the command itself. That is what
                    // `opens_a_block_scalar` enumerated by hand.
                    run: field(step, "run")
                        .and_then(Yaml::as_str)
                        .unwrap_or_default()
                        .to_string(),
                })
                .collect();
            jobs.push(Job {
                keys: keys_of(body),
                steps,
            });
        }
    }

    Workflow { triggers, jobs }
}

/// Does this workflow still run on a pull request at all?
///
/// The assumption the `ci.yml`-only scope rests on, and a fact about
/// the file rather than a given: if the triggers stop including
/// `pull_request`, the step gates nothing however it looks.
///
/// MATCHED WHOLE, against parsed trigger names. A substring search
/// over the `on:` block's text answered `true` for
/// `pull_request_review:` -- which fires on review events, not on a
/// pull request opening or being pushed to, so it gates nothing -- and
/// for `pull_request` inside a comment, including the comment that
/// says it was switched off.
///
/// # `pull_request_target` is NOT accepted, and that is a change
///
/// This guard used to accept it, on the reasoning that it also runs on
/// pull requests and can be a required check. That was #146. It runs
/// against the BASE repository with a write token and the repository's
/// secrets, and checks out the base ref by default, so a workflow
/// triggered only that way may never build the contributor's code at
/// all -- and accepting it as proof the merge is gated is permissive
/// in the worst direction for twelve library crates that take pull
/// requests from forks.
///
/// The alternative considered was to accept it conditionally, on
/// finding a checkout that names the pull request head. Both designs
/// refuse when they do not recognise the checkout, so both fail safe;
/// what settled it is that `pull_request_target` appears in ZERO of
/// the twelve repositories' workflows. The conditional branch would
/// guard a configuration that exists nowhere, and "we do not use this
/// trigger, and a test says so" is the better standing statement.
///
/// Note the narrowness of what this refuses: a workflow carrying BOTH
/// `pull_request:` and `pull_request_target:` -- the ordinary way to
/// reach secrets without giving up the gate -- is satisfied by the
/// former and never reaches this question.
fn runs_on_pull_request(wf: &Workflow) -> bool {
    wf.triggers.iter().any(|t| t == "pull_request")
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

/// Walk a workflow's steps and collect what `select` finds in each
/// `run:`.
///
/// `gating` restricts the walk to steps whose result the pull-request
/// gate actually reads: the workflow must still trigger on a pull
/// request, and neither the job nor the step may carry a key from
/// [`NON_GATING_KEYS`].
///
/// One walk, shared by both halves of the guard. BOTH HALVES ARE
/// STEP-AWARE, and that is deliberate. On the sibling `rust-fs-btrfs`
/// copy of this guard the headline assertion was left line-based while
/// only the handshake one was step-aware, so under `if: false` the
/// headline PASSED and its own failure message would have claimed the
/// pull-request gate could see an overflow when the step it names does
/// not run. Sharing the walk is what stops the two drifting apart
/// again, rather than fixing them separately twice.
fn scan_steps(workflow: &str, gating: bool, select: fn(&str) -> Vec<String>) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if gating && !runs_on_pull_request(&wf) {
        return Vec::new();
    }
    let carries_a_non_gating_key =
        |keys: &[String]| keys.iter().any(|k| NON_GATING_KEYS.contains(&k.as_str()));

    let mut out = Vec::new();
    for job in &wf.jobs {
        if gating && carries_a_non_gating_key(&job.keys) {
            continue;
        }
        for step in &job.steps {
            if gating && carries_a_non_gating_key(&step.keys) {
                continue;
            }
            out.extend(select(&step.run));
        }
    }
    out
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

    let debug_runs = gating_runs_with_overflow_checks(&workflow);
    assert!(
        !debug_runs.is_empty(),
        "no `cargo test` in {} runs without `--release` IN A STEP WHOSE \
         RESULT GATES A PULL REQUEST, so no defect whose \
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
fn debug_runs_that_prove_the_build_traps(script: &str) -> Vec<String> {
    runs_with_overflow_checks(script)
        .into_iter()
        .filter(|command| command.contains("EXPECT_OVERFLOW_CHECKS=1"))
        .collect()
}

/// The runs of steps that run in debug AND whose result ACTUALLY GATES
/// a pull request.
///
/// This is what the guard asks. [`runs_with_overflow_checks`] finds
/// the command; this asks whether anything reads its result. Adding
/// `if: false` to the guarded step in `ci.yml`, or
/// `continue-on-error: true`, left all 27 guard tests green while the
/// gate stopped gating. See #143.
fn gating_runs_with_overflow_checks(workflow: &str) -> Vec<String> {
    scan_steps(workflow, true, runs_with_overflow_checks)
}

/// The gating runs that also carry the handshake.
///
/// BOTH HALVES ARE STEP-AWARE, and that is deliberate. On the sibling
/// `rust-fs-btrfs` copy of this guard the headline assertion was left
/// line-based while only the handshake one was step-aware, so under
/// `if: false` the headline PASSED and its own failure message would
/// have claimed the pull-request gate could see an overflow when the
/// step it names does not run. Every defeat still turned that suite
/// red through the other assertion, so it was a precision defect
/// rather than a hole -- but it left the "runs without --release"
/// property verified line-based and defeatable if the handshake
/// assertion were ever weakened.
fn gating_runs_that_prove_the_build_traps(workflow: &str) -> Vec<String> {
    scan_steps(workflow, true, debug_runs_that_prove_the_build_traps)
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

    let proving = gating_runs_that_prove_the_build_traps(&workflow);
    assert!(
        !proving.is_empty(),
        "no `cargo test` in {} runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1 in a step whose result gates a pull \
         request, so nothing checks whether the profile \
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
/// Whether the step's result is READ, which the line-based parser
/// above cannot see. Six conditions, each with its own test, plus a
/// control asserting the unmodified shape IS counted so the others
/// cannot pass for the wrong reason.
mod gating {
    use super::gating_runs_that_prove_the_build_traps as gating;

    /// The shape that does gate. Every test below is this with one
    /// thing changed, so a failure here means the fixture is wrong
    /// rather than the property.
    const GATING: &str = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";

    const STEP: &str = "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n";

    #[test]
    fn the_control_shape_gates() {
        assert_eq!(
            gating(GATING).len(),
            1,
            "the control must be counted, or every test below passes for the wrong reason"
        );
    }

    #[test]
    fn a_step_carrying_if_does_not_gate() {
        for condition in [
            "if: false",
            "if: ${{ false }}",
            "if: github.event_name == 'push'",
            "if: ${{ env.SOMETHING == 'yes' }}",
        ] {
            let yaml = GATING.replace(STEP, &format!("{STEP}        {condition}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{condition}` may or may not run, so it cannot be what \
                 makes the gate able to see an overflow. Rejected on the key's presence \
                 rather than by evaluating it -- the spellings are open-ended."
            );
        }
    }

    #[test]
    fn a_step_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace(STEP, &format!("{STEP}        continue-on-error: true\n"));
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the step runs and its failure is discarded, which is this project's own \
             named defect: a step that runs and whose result nothing reads"
        );
    }

    #[test]
    fn a_job_carrying_if_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    if: false\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "the same reasoning one level up: a job that may not run cannot gate"
        );
    }

    #[test]
    fn a_job_carrying_continue_on_error_does_not_gate() {
        let yaml = GATING.replace("  test:\n", "  test:\n    continue-on-error: true\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "a job whose failure is discarded cannot gate, however sound its steps"
        );
    }

    /// The assumption the `ci.yml`-only scope rests on, which is a
    /// fact about the file rather than a given.
    #[test]
    fn a_workflow_that_no_longer_runs_on_pull_request_does_not_gate() {
        let yaml = GATING.replace(
            "  pull_request:\n    branches: [main]\n",
            "  push:\n    branches: [main]\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "scoping the scan to ci.yml assumes ci.yml is what runs on a pull request; \
             if its triggers stop including pull_request, the step gates nothing no \
             matter how it looks"
        );
    }

    /// `pull_request_target` IS A PULL-REQUEST TRIGGER, so the
    /// substring match in `runs_on_pull_request` counting it is
    /// deliberate rather than sloppy.
    ///
    /// Pinned because it reads like a bug and was mistaken for one
    /// while witnessing this fix: a mutation replacing `pull_request:`
    /// with `pull_request_target:` left the guard green and looked
    /// like a survivor. It is not -- such a workflow still runs on
    /// pull requests, in the base-repository context, and can still be
    /// a required check. The defeat that matters is the trigger going
    /// away, which the test above covers by removing it.
    #[test]
    fn a_pull_request_target_trigger_alone_does_not_gate() {
        let yaml = GATING.replace("  pull_request:\n", "  pull_request_target:\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            gating(&yaml).is_empty(),
            "pull_request_target runs against the BASE repository with a write token and \
             the repository's secrets, and checks out the base ref by default, so a \
             workflow triggered only that way may never build the contributor's code. \
             It is not proof that the merge is gated. See #146."
        );
    }

    /// THE CONTROL THAT STOPS THE REFUSAL OVER-CORRECTING.
    ///
    /// Carrying both triggers is the ordinary way to reach secrets
    /// without giving up the gate, and such a workflow IS gated -- by
    /// its `pull_request:` key, which the refusal above must not
    /// disturb. Without this test, narrowing the comparison to
    /// `t == "pull_request" && !any(t == "pull_request_target")` would
    /// pass every other assertion in this file while refusing a
    /// perfectly gated workflow. Contributed by the branch this change
    /// supersedes; it is the arm that branch added and the reason to
    /// keep it whatever the parser looks like.
    #[test]
    fn a_workflow_carrying_both_triggers_still_gates() {
        let yaml = GATING.replace(
            "  pull_request:\n",
            "  pull_request:\n  pull_request_target:\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the workflow still triggers on pull_request, so it still gates; refusing it \
             would be the over-correction"
        );
    }

    /// A `#` inside a quoted shell value is content, not a comment.
    ///
    /// This is what `without_comment` defended, and it is the reason
    /// that helper existed: a hand-rolled scanner has to decide where
    /// a comment starts, and its own doc conceded the rule was "crude
    /// next to real YAML". The parser decides it by the grammar --
    /// inside a quoted scalar a `#` is simply a character -- so the
    /// property is asserted here rather than left to a heuristic.
    #[test]
    fn a_hash_inside_a_quoted_value_is_content_not_a_comment() {
        let yaml = GATING.replace(
            "      - run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
            "      - run: echo '#1'; EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "the `#` is inside a quoted shell string, so the command after it is still \
             the command; treating it as a comment would refuse a correct workflow"
        );
    }

    /// A workflow the parser cannot read is a failure, never a pass.
    ///
    /// The direction matters: swallowing the error and returning an
    /// empty structure would report "no debug run gates this", which is
    /// also a failure and therefore safe -- but returning early with a
    /// pass would be the blindness this module exists to refuse.
    #[test]
    #[should_panic(expected = "not valid YAML")]
    fn a_workflow_that_does_not_parse_is_a_failure() {
        super::parse_workflow("jobs:\n  test:\n   - broken: [unclosed\n");
    }

    /// THE OTHER DIRECTION, which is the one that costs something.
    ///
    /// A step's `env:` mapping says nothing about whether its result
    /// is read, so it must NOT disqualify the step. Over-strictness
    /// here would be self-defeating: the handshake this guard looks
    /// for is itself an environment variable, and a maintainer moving
    /// it into a mapping would turn the guard red on a workflow that
    /// gates perfectly well. Pinned so a later tightening of the
    /// non-gating key list cannot quietly swallow it.
    #[test]
    fn a_step_carrying_an_env_mapping_still_gates() {
        let yaml = GATING.replace(
            STEP,
            "      - env:\n          CARGO_TERM_COLOR: always\n        run: EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
        );
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "an `env:` mapping is not a condition and does not discard a result, so the \
             step still gates. Rejecting it would be over-strict in the one direction \
             that breaks a working workflow."
        );
    }

    /// The HEADLINE half, step-aware as well -- not only the handshake
    /// one. This is the assertion that was left line-based on the
    /// sibling `rust-fs-btrfs` copy, where it passed under `if: false`
    /// while claiming the pull-request gate could see an overflow.
    #[test]
    fn the_headline_half_is_step_aware_too() {
        let yaml = GATING.replace(STEP, &format!("{STEP}        if: false\n"));
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert!(
            super::gating_runs_with_overflow_checks(&yaml).is_empty(),
            "a step carrying `if: false` must not count as a debug run either, or the \
             guard's own failure message names a step that does not run"
        );
        assert_eq!(
            super::gating_runs_with_overflow_checks(GATING).len(),
            1,
            "control: the unmodified shape must still count"
        );
    }

    /// A TRAILING COMMENT ON THE JOB LINE MUST NOT LOSE THE JOB.
    ///
    /// The job header is recognised by its line ending in a colon, and
    /// `  test:  # the gate` does not -- so the job's steps were never
    /// collected and the guard reported that nothing gates. Loud
    /// rather than silent, but wrong, and on a workflow that gates
    /// perfectly well. The over-strict direction is the safe one for a
    /// CONDITION; it is not safe for a comment.
    #[test]
    fn a_job_line_with_a_trailing_comment_still_gates() {
        let yaml = GATING.replace("  test:\n", "  test:  # the pull-request gate\n");
        assert_ne!(yaml, GATING, "the mutation must actually apply");
        assert_eq!(
            gating(&yaml).len(),
            1,
            "a comment after the job's name says nothing about whether its result is read"
        );
    }

    /// A COMMENT IS NOT A TRIGGER. The substring search this replaced
    /// answered `true` for the comment that says the trigger was
    /// switched off, which is the most likely place the word appears
    /// on a workflow that no longer gates.
    #[test]
    fn a_commented_out_pull_request_trigger_does_not_gate() {
        for spelling in [
            "  push:\n    branches: [main]\n  # pull_request disabled for now\n",
            "  push:\n    branches: [main]\n    # was: pull_request\n",
            "  push: # replaces pull_request\n    branches: [main]\n",
        ] {
            let yaml = GATING.replace("  pull_request:\n    branches: [main]\n", spelling);
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                yaml.contains("pull_request"),
                "precondition: the word must still be PRESENT, or this tests nothing -- \
                 the whole point is text that mentions it while not triggering on it"
            );
            assert!(
                gating(&yaml).is_empty(),
                "a workflow whose only mention of pull_request is a comment gates \
                 nothing. Spelling: {spelling:?}"
            );
        }
    }

    /// A DIFFERENT TRIGGER THAT STARTS THE SAME WAY IS A DIFFERENT
    /// TRIGGER. `pull_request_review` fires on review events, not on a
    /// pull request opening or being pushed to, so a step under it
    /// cannot be what gates the pull request.
    ///
    /// This is why the check matches whole names and lists
    /// `pull_request_target` explicitly rather than matching a prefix.
    #[test]
    fn a_similarly_named_trigger_does_not_gate() {
        for trigger in [
            "pull_request_review",
            "pull_request_review_comment",
            "pull_requests",
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "`{trigger}` is not `pull_request`, and a substring match said it was"
            );
        }
    }

    /// A QUOTED KEY IS THE SAME KEY. `"if": false` is valid YAML and
    /// GitHub Actions honours it exactly as `if: false`, but a raw
    /// text compare against `if` matched neither quoted spelling -- so
    /// a step that does not gate was counted as one that does.
    ///
    /// This file's manifest parser already normalises quotes, after a
    /// quoted `"overflow-checks" = false` defeated that scan. Same
    /// defect one format across.
    #[test]
    fn a_quoted_non_gating_key_still_does_not_gate() {
        for key in [
            "\"if\": false",
            "'if': false",
            "\"continue-on-error\": true",
            "'continue-on-error': true",
        ] {
            let yaml = GATING.replace(STEP, &format!("{STEP}        {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a step carrying `{key}` does not gate, and the quotes do not change that"
            );
        }
    }

    /// The same, one level up, where the job-level key extraction had
    /// the identical bypass.
    #[test]
    fn a_quoted_non_gating_key_on_the_job_still_does_not_gate() {
        for key in ["\"if\": false", "'continue-on-error': true"] {
            let yaml = GATING.replace("  test:\n", &format!("  test:\n    {key}\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            assert!(
                gating(&yaml).is_empty(),
                "a job carrying `{key}` does not gate, quoted or not"
            );
        }
    }

    /// EVERY BLOCK-SCALAR SPELLING IS A BLOCK. `|` is not the only
    /// one: YAML's chomping and indentation indicators all open a
    /// block, and treating one as the command itself meant the block's
    /// contents were never read -- so changing `|` to `|-`, which
    /// preserves behaviour, made the guard report that nothing gated.
    #[test]
    fn a_run_block_is_read_whole_in_every_block_scalar_spelling() {
        for indicator in ["|", "|-", "|+", ">", ">-", ">+", "|2", ">2-"] {
            let yaml = format!(
                "on:\n  pull_request:\n    branches: [main]\njobs:\n  test:\n    steps:\n\
                 {}      - name: a block\n        run: {indicator}\n          set -euo pipefail\n\
                 {}          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib\n",
                "", ""
            );
            assert_eq!(
                gating(&yaml).len(),
                1,
                "`run: {indicator}` opens a block, so the command inside it must be seen"
            );
        }
    }

    /// The flow-sequence and single-scalar spellings of `on:`, which
    /// put the triggers on the same line rather than in a block.
    #[test]
    fn the_inline_trigger_spellings_are_read_too() {
        let base = GATING.replace("on:\n  pull_request:\n    branches: [main]\n", "");
        for (spelling, gates) in [
            ("on: [push, pull_request]\n", true),
            ("on: [push]\n", false),
            ("on: pull_request\n", true),
            ("on: push\n", false),
            ("on:\n  - push\n  - pull_request\n", true),
            ("on:\n  - push\n", false),
        ] {
            let yaml = format!("{spelling}{base}");
            assert_eq!(
                !gating(&yaml).is_empty(),
                gates,
                "`{spelling:?}` should {} gate",
                if gates { "" } else { "not" }
            );
        }
    }

    /// A `run: |` block is read whole, so a command inside a shell
    /// loop is visible rather than seen as fragments.
    #[test]
    fn a_run_block_is_read_whole() {
        let yaml = "\
on:
  pull_request:
    branches: [main]
jobs:
  test:
    steps:
      - name: a block
        run: |
          set -euo pipefail
          EXPECT_OVERFLOW_CHECKS=1 cargo test --locked --lib
";
        assert_eq!(
            gating(yaml).len(),
            1,
            "a command inside a `run: |` block must be seen; several steps in this \
             repository's ci.yml are blocks like this one, including two shell loops"
        );
    }

    /// The real `ci.yml` gates, read through the same function the
    /// guard uses. Distinct from the guard's own assertion: this one
    /// proves the PARSER copes with the real file's shape -- matrix
    /// strategies, `uses:`/`with:` mappings, comments between steps --
    /// rather than only with the fixtures above.
    #[test]
    fn the_real_ci_yml_still_parses_into_a_gating_step() {
        let workflow = super::read_or_panic(
            &super::manifest_dir()
                .join(".github")
                .join("workflows")
                .join("ci.yml"),
        );
        assert!(
            !gating(&workflow).is_empty(),
            "the real ci.yml must parse into at least one gating step, or the guard is \
             passing on a fixture and failing on the file it exists to read"
        );
    }
}

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
