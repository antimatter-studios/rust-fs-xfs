//! The debug test run, and the independent oracle, guard themselves.
//!
//! The suite runs twice: once `--release`, through
//! `scripts/ci-test.sh`, and once in the debug profile, which is
//! `chores.yml`'s `test:unit` task. The second run looks redundant and
//! is not. Overflow checks are on in debug and off in release, so a
//! defect whose only symptom is an arithmetic overflow panic cannot be
//! observed by a release-only gate — which is what this crate had, in
//! every workflow it has had: `ci.yml` was `--release` at both
//! invocations, `scripts/ci-test.sh` fixes `--release` for the whole
//! suite, and `release.yml` runs no `cargo test` at all.
//!
//! THE COMMAND MOVED; THE PROPERTY DID NOT. Every CI job now runs a
//! chore task and `ci.yml` contains no `cargo test` line at all. The
//! debug run — the absent `--release` and the
//! `EXPECT_OVERFLOW_CHECKS=1` handshake `src/lib.rs` reads — lives in
//! `chores.yml` under `test:unit`, and the workflow's contribution is
//! that a job whose result gates a pull request invokes that task.
//! Neither half is worth anything alone: a task that builds in debug
//! and is never run gates nothing, and a job that runs a task which has
//! quietly acquired `--release` gates nothing either. So the two are
//! asserted separately, because they fail separately and the reader
//! needs to be sent to the right file.
//!
//! The comments above both explain the reasoning, but a comment is
//! advice. Deleting either leaves CI green, saves a compile, and puts
//! the blindness back. The failure mode is a well-intentioned tidy-up:
//! nobody removes a test job on purpose, they consolidate two lines that
//! appear to do the same thing.
//!
//! So the property gets a check of its own. It asserts that at least one
//! `cargo test` in the task still runs without `--release`, and that at
//! least one gating job still invokes the task, rather than that any
//! particular line is present — so renaming or reformatting either does
//! not defeat it while a deletion does.
//!
//! # The jobs that run the INDEPENDENT oracle guard themselves too
//!
//! #208. The evidence this crate rests on is not its own: it is
//! xfsprogs reading back what the driver wrote, and the kernel mounting
//! it. Nothing used to fail if the job that ran that oracle was
//! renamed, given an `if:`, given `continue-on-error:`, stopped
//! installing its tool, or stopped running its suite — any one of which
//! leaves a green check whose name still reads like cross-validation
//! while nothing is cross-validated. rust-img-qcow2#97 is what that
//! looks like once it has happened: a probe that returned early on
//! every runner, in every job, on every push and pull request, while
//! the test reported `ok`. An executed-test floor cannot see it (an
//! early return still counts as passed) and a required check cannot
//! either (the job still reports green under the required name).
//!
//! The shape rust-img-qcow2#102 added is ported here to this
//! repository's new one, and the facts are again asserted one test
//! apiece: the workflow still triggers on `pull_request`; a gating job
//! still builds the fixtures; a gating job still runs the whole suite;
//! the oracle tools are still installed where the tests reach them,
//! which is now the guest's provisioning rather than an `apt-get`
//! step; the runner's own xfsprogs is still made unusable in the job
//! that runs the suite; `ci-ok` still aggregates every job and still
//! fails on a skip; and `.github-guard` still requires it.
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
//! It therefore runs in whichever tier selects it — including the
//! release one — rather than only in the debug run it protects. That is
//! fine and deliberate: what it reads is a handful of text files, so
//! the profile it is built in is irrelevant to its result.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The workflow this repository's pull requests are gated by.
///
/// `ci.yml` specifically, not every workflow. `stress.yml` is scheduled
/// and `release.yml` runs no tests, so a debug run or an oracle job in
/// either would satisfy a broader scan without covering a single pull
/// request.
fn ci_yml() -> PathBuf {
    manifest_dir()
        .join(".github")
        .join("workflows")
        .join("ci.yml")
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

/// Every `cargo test` invocation in a script that would be compiled
/// with overflow checks on.
///
/// The subject is `chores.yml`'s `test:unit` now -- `ci.yml` has no
/// `cargo test` line left -- but the rules are unchanged, and so is
/// the reason for each. Four things disqualify a line, and each one is
/// a way the guard could otherwise be satisfied by something that does
/// not actually build in debug:
///
/// - it is a comment. This is not defensive here, it is load bearing:
///   the block above `test:unit` discusses the profile, the handshake
///   and `ci-test.sh` at length, and `ci.yml` used to quote
///   `cargo test --locked --lib` verbatim inside the comment
///   explaining the step it has since lost -- so a scan that ignored
///   comments would still find a run after the run itself had been
///   deleted, and would pass. [`task_commands`] parses the manifest,
///   which removes the comments before this sees anything; the rule
///   stays because this function is also what reads a workflow's
///   `run:` blocks, where the comments are the script's own;
/// - it is an inline trailing comment on an otherwise-`--release` line;
/// - it passes `--release`, or names a profile explicitly;
/// - it sets a `CARGO_PROFILE_*` variable, which can turn overflow
///   checks off for the dev profile from outside the manifest.
///
/// A line invoking `scripts/ci-test.sh` is not counted either, and needs
/// no rule of its own: the script supplies `cargo test --release`
/// itself, so no literal `cargo test` appears in the caller. That
/// premise is asserted by [`ci_test_sh_still_supplies_release_itself`]
/// rather than assumed -- including for the `--gate` mode `test:unit`
/// calls, which invokes no cargo at all.
fn runs_with_overflow_checks(script: &str) -> Vec<String> {
    cargo_test_commands(script)
        .into_iter()
        .filter(|command| {
            !(command.contains("--release")
                || command.contains("--profile")
                || command.contains("CARGO_PROFILE_")
                || selects_release_by_short_flag(command))
        })
        .collect()
}

/// Every `cargo test` invocation in a script, whatever profile it
/// selects.
///
/// The unfiltered half of [`runs_with_overflow_checks`], and it is
/// separate because the `test:unit` guard asks a question the filtered
/// list cannot answer: not "is there a debug run" but "is EVERY cargo
/// run in this task a debug one". Comparing the two lists is what
/// makes a `--release` added beside the debug line a failure rather
/// than a no-op, which is the likeliest way that task acquires one.
///
/// The comment rules are the ones described above: a line that is a
/// comment is not a run, and a trailing comment is not part of the
/// command.
fn cargo_test_commands(script: &str) -> Vec<String> {
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
            Some(command.to_string())
        })
        .collect()
}

/// Every non-comment line of `script` that invokes `chore <task>`.
///
/// The workflow no longer runs `cargo test`: it runs chore tasks, and
/// what a job contributes to the gate is WHICH task it invokes. So the
/// scan that used to look for a cargo command looks for a task name,
/// through the same shell splitter ([`shell_commands`]) and with the
/// same rules about comments — `ci.yml`'s prose names every task it
/// runs, and a scan that counted the prose would still find
/// `chore test:unit` after the step had been deleted.
///
/// THE TASK IS THE FIRST ARGUMENT THAT IS NOT A FLAG, which is how
/// chore reads it, and it is compared WHOLE: `chore test:unit` is not
/// `chore test`, and counting one as the other would let the aggregate
/// tier stand in for the debug one. Anything after `--` belongs to the
/// task rather than to chore, so it is not a task name.
fn runs_chore_task(script: &str, task: &str) -> Vec<String> {
    script
        .lines()
        .filter_map(|raw| {
            let line = raw.trim_start();
            if line.starts_with('#') {
                return None;
            }
            let command = line.split(" #").next().unwrap_or(line).trim();
            shell_commands(command)
                .iter()
                .any(|words| chore_task_of(words) == Some(task))
                .then(|| command.to_string())
        })
        .collect()
}

/// The task one command invokes, if the command is a `chore` run.
///
/// `chore` by name or by path, after any `NAME=value` assignments the
/// shell applies rather than runs — the same reading
/// [`selects_release_by_short_flag`] does for cargo, and for the same
/// reason: in `echo chore test` the program is `echo`.
fn chore_task_of(words: &[String]) -> Option<&str> {
    let at = words.iter().position(|w| !is_assignment(w))?;
    let program = words[at].as_str();
    if program != "chore" && !program.ends_with("/chore") {
        return None;
    }
    words[at + 1..]
        .iter()
        .map(String::as_str)
        .take_while(|&w| w != "--")
        .find(|w| !w.starts_with('-'))
}

/// Whether `command` runs `cargo test` with the release profile selected
/// by its short flag (#159).
///
/// `cargo test -r` is `cargo test --release`, and the string check above
/// does not see it. Nor is it one spelling: clap merges short flags, so
/// `-qr` and `-rq` carry it as well, anywhere before `--`. A cluster ends
/// at a short option that takes a value -- `-p`, `-j`, `-F` or `-Z` --
/// whose value is the rest of the word or, when the word ends there, the
/// next one: `-j4 -r` is release, `-pr` names a package `r`. Everything
/// after `--` belongs to the test harness, where `-r` is not cargo's.
fn selects_release_by_short_flag(command: &str) -> bool {
    shell_commands(command).iter().any(|command| {
        let words: Vec<&str> = command.iter().map(String::as_str).collect();
        // THE COMMAND WORD, NOT ANY WORD (Greptile on #181). In
        // `echo cargo test -r` the program is `echo`, and a scan that took
        // `cargo` wherever it appeared discarded a debug run beside it. The
        // program is the first word after any `NAME=value` assignments.
        let at = words
            .iter()
            .position(|w| !is_assignment(w))
            .unwrap_or(words.len());
        let Some(&program) = words.get(at) else {
            return false;
        };
        // `cargo` by name or by path, then any `+toolchain`, then `test`.
        if program != "cargo" && !program.ends_with("/cargo") {
            return false;
        }
        let mut next = at + 1;
        while words.get(next).is_some_and(|w| w.starts_with('+')) {
            next += 1;
        }
        words.get(next) == Some(&"test") && release_in(&words[next + 1..])
    })
}

/// Whether `word` is a `NAME=value` assignment, which the shell applies to
/// the command that follows rather than running.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        name.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// `command` split into the commands the shell's control operators --
/// `&&`, `||`, `;`, `|` and `&` -- separate, each as its words, with
/// quotes and backslashes removed as the shell removes them.
///
/// Operators need no spaces: `true&&cargo test -r` is a `cargo test` run,
/// and in `cargo test --lib&&rm -rf build` the `-rf` is `rm`'s. An `&` or
/// `|` straight after `>` or `<` is part of a redirection (`2>&1`, `>|`).
/// Inside quotes, or after a backslash, nothing is an operator or a word
/// break, and what the quotes held is the word: `--features 'a;b' -r` is
/// one command, and `'-r'` is `-r`.
fn shell_commands(command: &str) -> Vec<Vec<String>> {
    let mut commands = vec![Vec::new()];
    let mut word: Option<String> = None;
    let mut chars = command.chars().peekable();
    let mut previous = None;
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                let word = word.get_or_insert_with(String::new);
                word.extend(chars.by_ref().take_while(|&q| q != '\''));
            }
            '"' => {
                let word = word.get_or_insert_with(String::new);
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' if matches!(chars.peek(), Some('"' | '\\' | '$' | '`')) => {
                            word.extend(chars.next());
                        }
                        _ => word.push(q),
                    }
                }
            }
            '\\' => word.get_or_insert_with(String::new).extend(chars.next()),
            ';' | '&' | '|' if !matches!(previous, Some('>' | '<')) => {
                if c != ';' && chars.peek() == Some(&c) {
                    chars.next();
                }
                commands.last_mut().unwrap().extend(word.take());
                commands.push(Vec::new());
            }
            c if c.is_whitespace() => commands.last_mut().unwrap().extend(word.take()),
            c => word.get_or_insert_with(String::new).push(c),
        }
        previous = Some(c);
    }
    commands.last_mut().unwrap().extend(word.take());
    commands
}

/// Whether `cargo test`'s `arguments`, up to the end of its own command,
/// carry `-r`. See [`selects_release_by_short_flag`].
///
/// `arguments` are one command's words ([`shell_commands`]), so in
/// `cargo test --lib && rm -rf build` the `r` in `-rf` is not among them.
fn release_in(arguments: &[&str]) -> bool {
    const LONG_OPTIONS_TAKING_A_VALUE: [&str; 15] = [
        "--package",
        "--exclude",
        "--features",
        "--target",
        "--target-dir",
        "--manifest-path",
        "--profile",
        "--test",
        "--bin",
        "--example",
        "--bench",
        "--jobs",
        "--message-format",
        "--color",
        "--config",
    ];
    let mut next_is_a_value = false;
    for &argument in arguments {
        if std::mem::take(&mut next_is_a_value) {
            continue;
        }
        if argument == "--" {
            return false;
        }
        if argument.starts_with("--") {
            next_is_a_value =
                !argument.contains('=') && LONG_OPTIONS_TAKING_A_VALUE.contains(&argument);
        } else if let Some(cluster) = argument.strip_prefix('-') {
            for (at, flag) in cluster.char_indices() {
                match flag {
                    'r' => return true,
                    'p' | 'j' | 'F' | 'Z' => {
                        next_is_a_value = at + 1 == cluster.len();
                        break;
                    }
                    _ => {}
                }
            }
        }
    }
    false
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
    /// The key under `jobs:`, which is what another job's `needs:`
    /// names and what a check run is called when the job has no
    /// `name:`.
    id: String,
    /// The `name:`, which is what GitHub reports as the check-run name
    /// and therefore what branch protection can require. Absent means
    /// the id is the name.
    name: Option<String>,
    /// The jobs this one waits for, in either spelling: a sequence or
    /// a single scalar. `ci-ok` is nothing but this list, so a job
    /// added to the file and forgotten here is exactly the hole the
    /// aggregate exists to close.
    needs: Vec<String>,
    /// The `if:` value, kept rather than only counted, because
    /// `ci-ok` is REQUIRED to carry one — `always()`, without which it
    /// is skipped along with whatever it was aggregating. Everywhere
    /// else the presence of the key is what matters and the value is
    /// deliberately not read; see [`NON_GATING_KEYS`].
    condition: Option<String>,
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
        for (id, body) in mapping.iter() {
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
            // `needs:` takes two legal shapes, a sequence or a single
            // scalar, and both are lists of job ids.
            let needs = match field(body, "needs") {
                Some(needs) if needs.as_sequence().is_some() => needs
                    .as_sequence()
                    .into_iter()
                    .flatten()
                    .filter_map(|job| job.as_str().map(str::to_string))
                    .collect(),
                Some(needs) => needs.as_str().map(str::to_string).into_iter().collect(),
                None => Vec::new(),
            };
            jobs.push(Job {
                id: id.as_str().unwrap_or_default().to_string(),
                name: field(body, "name")
                    .and_then(Yaml::as_str)
                    .map(str::to_string),
                needs,
                condition: field(body, "if").and_then(Yaml::as_str).map(str::to_string),
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

/// Why `workflow` gates no pull request at all, or `None` if it does.
///
/// The real-file assertions below ask this FIRST. Without it, a
/// workflow whose `on:` block moved reported "no debug `cargo test`
/// gates a pull request", which sends the reader to a step that is
/// fine (#153). This names the triggers that were found instead, and
/// says why `pull_request_target` alone does not count.
fn not_a_pull_request_gate(workflow: &str) -> Option<String> {
    let wf = parse_workflow(workflow);
    if runs_on_pull_request(&wf) {
        return None;
    }
    let mut why = format!(
        "the workflow does not trigger on `pull_request` at all (its triggers: {:?}), so \
         none of its steps gates a pull request however they are written. The steps are \
         not the problem; the `on:` block is.",
        wf.triggers
    );
    if wf.triggers.iter().any(|t| t == "pull_request_target") {
        why.push_str(
            " `pull_request_target` alone is refused on purpose: it runs against the base \
             repository and may never build the contributor's code. See \
             `runs_on_pull_request`; carry `pull_request:` beside it.",
        );
    }
    Some(why)
}

/// Keys whose presence on a step or job means its result does not gate.
const NON_GATING_KEYS: [&str; 2] = ["if", "continue-on-error"];

/// Whether a job's or a step's keys include one from
/// [`NON_GATING_KEYS`].
///
/// A free function rather than a closure inside the walk, because the
/// job-level guards ask it too: #208 wants the JOB that runs the
/// oracle, not merely a step somewhere, and both have to read
/// "gating" the same way or the two answers drift.
fn carries_a_non_gating_key(keys: &[String]) -> bool {
    keys.iter().any(|k| NON_GATING_KEYS.contains(&k.as_str()))
}

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
fn scan_steps(workflow: &str, gating: bool, select: impl Fn(&str) -> Vec<String>) -> Vec<String> {
    let wf = parse_workflow(workflow);
    if gating && !runs_on_pull_request(&wf) {
        return Vec::new();
    }
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

/// The jobs whose result gates a pull request and one of whose gating
/// steps runs `chore <task>`.
///
/// The step-level walk above answers "does the gate run this
/// anywhere"; this answers "which job does", which is the question
/// #208 asks. The oracle guards need the job itself: the step that
/// makes the runner's own xfsprogs unusable is only evidence if it is
/// in the SAME job as the suite it is protecting, and a failure
/// message that cannot name the job sends the reader to the wrong
/// half of a 300-line workflow.
fn gating_jobs_running<'a>(wf: &'a Workflow, task: &str) -> Vec<&'a Job> {
    if !runs_on_pull_request(wf) {
        return Vec::new();
    }
    wf.jobs
        .iter()
        .filter(|job| {
            !carries_a_non_gating_key(&job.keys)
                && job.steps.iter().any(|step| {
                    !carries_a_non_gating_key(&step.keys)
                        && !runs_chore_task(&step.run, task).is_empty()
                })
        })
        .collect()
}

/// The runs, in steps that gate a pull request, that invoke
/// `chore <task>`.
fn gating_chore_runs(workflow: &str, task: &str) -> Vec<String> {
    scan_steps(workflow, true, |script| runs_chore_task(script, task))
}
/// The commands of one task in `chores.yml`, as chore would run them.
///
/// PARSED, NOT SCANNED, and for the reason
/// [`runs_with_overflow_checks`] gives one file across: the comment
/// block above `test:unit` discusses `--release`,
/// `EXPECT_OVERFLOW_CHECKS` and `ci-test.sh` at length, so a text scan
/// would read the prose explaining the task as the task itself and
/// keep passing after the commands were gone. The parser drops every
/// comment before this sees anything.
///
/// A `cmds:` entry is a scalar or a mapping carrying `cmd:`; both are
/// commands. A `task:` reference is deliberately NOT followed: its
/// commands belong to the task it names, so a `test:unit` that
/// delegated its cargo run elsewhere returns nothing here and fails
/// the guard rather than being credited with a run this cannot see.
/// Red is the safe direction and the message says which file to open.
fn task_commands(manifest: &str, task: &str) -> Vec<String> {
    let documents = Yaml::load_from_str(manifest).unwrap_or_else(|e| {
        panic!(
            "chores.yml is not valid YAML: {e}. This guard reads the manifest rather \
             than scanning its text, so a file it cannot parse is a failure and never \
             a pass."
        )
    });
    let Some(document) = documents.first() else {
        return Vec::new();
    };
    let Some(body) = field(document, "tasks").and_then(|tasks| field(tasks, task)) else {
        return Vec::new();
    };
    field(body, "cmds")
        .and_then(Yaml::as_sequence)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            entry
                .as_str()
                .or_else(|| field(entry, "cmd").and_then(Yaml::as_str))
                .map(str::to_string)
        })
        .collect()
}

/// THE FIRST HALF OF THE OVERFLOW GUARD: the task still builds in a
/// profile that can see one.
///
/// The debug run is `chores.yml`'s now — `ci.yml` has no `cargo test`
/// line left — so this is where the absence of `--release` has to be
/// asserted. Without it, `test:unit` acquiring `--release` (the
/// obvious way to make a slow tier faster, and it would look like
/// every other tier here) leaves the workflow untouched, every job
/// green, and no defect whose only symptom is an arithmetic overflow
/// panic observable anywhere in this repository.
///
/// EVERY cargo run in the task is checked, not merely one of them. A
/// guard satisfied by the presence of a debug line would be satisfied
/// by a task that runs the suite twice, once in each profile, which is
/// not what `test:unit` is for and is a compile nobody chose to pay
/// for.
#[test]
fn the_unit_task_still_runs_cargo_in_a_profile_that_can_see_an_overflow() {
    let path = manifest_dir().join("chores.yml");
    let manifest = read_or_panic(&path);
    let cmds = task_commands(&manifest, "test:unit");

    // Non-emptiness first, twice over. `all()` over nothing is true,
    // and a `test:unit` that had lost its commands — or been renamed —
    // would satisfy the comparison below while establishing nothing.
    assert!(
        !cmds.is_empty(),
        "{}: the task `test:unit` has no commands, or no longer exists under that \
         name. It is the debug, overflow-checks-trapping run of this repository's \
         gate, and ci.yml's `unit`, `test-arm64` and `test-darwin` jobs invoke it by \
         name.",
        path.display()
    );
    let invocations: Vec<String> = cmds.iter().flat_map(|c| cargo_test_commands(c)).collect();
    assert!(
        !invocations.is_empty(),
        "{}: `test:unit` no longer invokes `cargo test` at all, so the tier the gate \
         runs for its overflow checks compiles nothing. Its commands are {cmds:?}.",
        path.display()
    );

    let debug = cmds
        .iter()
        .flat_map(|c| runs_with_overflow_checks(c))
        .collect::<Vec<String>>();
    let release: Vec<&String> = invocations.iter().filter(|c| !debug.contains(c)).collect();
    assert!(
        release.is_empty(),
        "{}: `test:unit` selects the release profile in {release:?}. Overflow checks \
         are on in debug and off in release, so this is the one tier that can observe \
         a defect whose only symptom is an arithmetic overflow panic — and the rest of \
         the suite already runs `--release` through scripts/ci-test.sh. If the debug \
         run looked redundant beside it, it is not; see the comment above the task.",
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
/// [`runs_with_overflow_checks`] finds the command; this asks whether
/// anything reads its result. Adding `if: false` to the guarded step
/// in `ci.yml`, or `continue-on-error: true`, left all 27 guard tests
/// green while the gate stopped gating. See #143.
///
/// The workflow no longer carries the command itself — it invokes
/// `chore test:unit`, and [`gating_chore_runs`] is what asks the same
/// question about that. This is kept, and pinned by `mod gating`,
/// because it is the shape both halves of the walk are held to: a
/// `cargo test` appearing in a workflow step again must be read
/// step-aware from the first day, not line-based until somebody
/// notices.
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
/// `CARGO_PROFILE_TEST_OVERFLOW_CHECKS` variable exported around the
/// task — by a job-level `env:` in the workflow, which the parser
/// cannot see because it only reads `run:` lines, or by a `lifecycle:`
/// or a wrapper script — and a `.cargo/config.toml`, which nothing
/// here reads. Both leave the debug run present, running, green and
/// blind.
///
/// All six are the same shape: a scanner enumerating the ways a thing
/// can be disabled, in the places it happens to look. Another pass buys
/// the next one. So the question is put to the build instead -- perform
/// an overflow, see whether you are stopped -- and this test's job
/// shrinks to making sure the gate still asks it.
#[test]
fn the_unit_task_still_asks_the_build_to_prove_it_traps_overflows() {
    let path = manifest_dir().join("chores.yml");
    let manifest = read_or_panic(&path);
    let cmds = task_commands(&manifest, "test:unit");
    assert!(
        !cmds.is_empty(),
        "control: {} must declare a `test:unit` with commands, or the assertion \
         below passes over an empty list",
        path.display()
    );

    let proving: Vec<String> = cmds
        .iter()
        .flat_map(|c| debug_runs_that_prove_the_build_traps(c))
        .collect();
    assert!(
        !proving.is_empty(),
        "no `cargo test` in `test:unit` ({}) runs without `--release` while setting \
         EXPECT_OVERFLOW_CHECKS=1, so nothing checks whether the profile the gate \
         builds actually traps an arithmetic overflow. Reading Cargo.toml is not \
         enough: the checks can also be turned off by a \
         CARGO_PROFILE_TEST_OVERFLOW_CHECKS variable in the task's environment, or by \
         a .cargo/config.toml, neither of which is in any file this test reads. The \
         handshake is what arms the one check that cannot be fooled by where the \
         setting lives. Its commands are {cmds:?}.",
        path.display()
    );
}

/// THE SECOND HALF OF THE OVERFLOW GUARD: the gate still runs the
/// task.
///
/// `chores.yml` can be as careful as it likes about the profile and
/// buy nothing if no pull request runs `test:unit`. The task is
/// invoked from three jobs, none of which has to exist: consolidating
/// them into one `chore test` would look like a simplification —
/// `test:native` runs `test:unit` too — and would leave the debug tier
/// running only where the whole suite runs, which is the x86_64 job
/// with the VM, and not at all on the two runners that have no KVM.
///
/// Asserted separately from the profile half because they fail
/// separately and in different files: this one is about `ci.yml`, that
/// one about `chores.yml`, and a single message covering both would
/// send half its readers to the wrong one.
#[test]
fn the_pull_request_gate_still_runs_the_debug_unit_task() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    if let Some(why) = not_a_pull_request_gate(&workflow) {
        panic!("{}: {why}", path.display());
    }
    let wf = parse_workflow(&workflow);
    assert!(
        !wf.jobs.is_empty(),
        "control: {} must parse into at least one job",
        path.display()
    );

    let jobs = gating_jobs_running(&wf, "test:unit");
    assert!(
        !jobs.is_empty(),
        "no job in {} runs `chore test:unit` IN A STEP WHOSE RESULT GATES A PULL \
         REQUEST, so the debug, overflow-checks-trapping tier is not part of this \
         repository's gate however carefully chores.yml defines it. A job or step \
         carrying `if:` or `continue-on-error:` does not count: its result is not \
         read. The jobs found were {:?}.",
        path.display(),
        wf.jobs.iter().map(|j| &j.id).collect::<Vec<_>>()
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

// ======================================================================
// THE INDEPENDENT ORACLE GUARDS ITSELF (#208).
//
// Everything above is this crate marking its own homework: a profile,
// a handshake, a test it wrote. What makes the suite worth anything is
// the tier where something else grades it -- xfsprogs reading back
// what the driver wrote, and the kernel mounting it -- and until #208
// nothing failed if that tier stopped happening. The job could be
// renamed, given an `if:`, given `continue-on-error:`, stop installing
// its tool or stop running its suite, and the check would still report
// green under a name that reads like cross-validation.
//
// The facts below are asserted ONE TEST APIECE because they fail
// separately, and each message has to name which of them went: a
// single "the oracle gate is broken" sends the reader to a 376-line
// workflow with nothing to look for.
// ======================================================================

/// The tools the oracle tiers call, and the ones the kernel tiers need
/// beside them.
///
/// Named here rather than derived, because that is the point: these
/// are the programs whose ANSWERS the suite treats as truth, and the
/// guards below hold both ends of them -- installed in the guest,
/// where every host and every runner gets the same version, and
/// unusable on the runner, where a stray call would be answered by
/// whatever Ubuntu ships that month.
const ORACLE_TOOLS: [&str; 5] = ["mkfs.xfs", "xfs_db", "xfs_repair", "xfs_logprint", "xfs_io"];

/// A shell script's commands: comment lines dropped and backslash
/// continuations joined.
///
/// Both matter for the files below. `vm-setup.sh` installs the oracle
/// tools with an `apt-get install` spread over three lines, so a
/// line-at-a-time scan sees the verb and the package list separately
/// and can conclude neither; and both scripts explain themselves at
/// length in comments that name every tool, so a scan counting those
/// would still pass after the commands had gone. That is the same trap
/// [`runs_with_overflow_checks`] documents for `ci.yml`.
///
/// Only WHOLE-LINE comments are dropped. A `#` mid-line can be inside
/// a quoted string -- `printf '#!/bin/sh\n...'` is in the very step
/// this reads -- and deciding where a shell comment starts is the
/// heuristic this file has already paid for once.
fn shell_lines(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for raw in script.lines() {
        let line = raw.trim();
        if line.starts_with('#') || (line.is_empty() && pending.is_none()) {
            continue;
        }
        let (text, continues) = match line.strip_suffix('\\') {
            Some(head) => (head.trim_end(), true),
            None => (line, false),
        };
        let mut joined = pending.take().unwrap_or_default();
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push_str(text);
        if continues {
            pending = Some(joined);
        } else {
            out.push(joined);
        }
    }
    out.extend(pending);
    out
}

/// Whether `text` names `word` as a word, rather than inside a longer
/// one.
///
/// `xfsprogs` is a substring of `xfsprogs-parent` and of
/// `xfsprogs-6.13.0.tar.xz`, both of which `vm-setup.sh` mentions
/// while installing neither, and `xfs_db` is a substring of
/// `xfs_dbwrapper`. A substring test would report a package installed
/// because its source tarball is downloaded. The word characters here
/// include `.` and `-` because the tools are spelled with them.
fn names_word(text: &str, word: &str) -> bool {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-'))
        .any(|found| found == word)
}

/// The programs a script looks up with `command -v`, whether directly
/// or as a `for` loop's list.
///
/// Two scripts are read this way and both spell it as a loop: the
/// guest's provisioning, which proves each tool present after
/// installing it, and the workflow step that takes the runner's own
/// copies away. Collecting the loop's LIST rather than searching the
/// file's text is what makes dropping one tool from it a failure --
/// `mkfs.xfs` and `xfs_db` are named elsewhere in both scripts, so a
/// text search would still find them after the check that matters had
/// stopped covering them.
///
/// The loop's body has to do the lookup for its list to count, which
/// is what tells `for tool in ...; do command -v "$tool"; done` apart
/// from any other loop over the same names.
fn tools_resolved_with_command_v(script: &str) -> Vec<String> {
    let lines = shell_lines(script);
    let mut out: Vec<String> = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        if line.contains("command -v") {
            out.extend(line.split_whitespace().map(str::to_string));
        }
        let Some((_, list)) = line
            .strip_prefix("for ")
            .and_then(|rest| rest.split_once(" in "))
        else {
            continue;
        };
        let body_looks_them_up = lines[at + 1..]
            .iter()
            .take_while(|l| !l.starts_with("done"))
            .any(|l| l.contains("command -v"));
        if body_looks_them_up {
            out.extend(
                list.split(|c: char| c.is_whitespace() || c == ';')
                    .filter(|word| !word.is_empty() && *word != "do")
                    .map(str::to_string),
            );
        }
    }
    out
}

/// The `[setup] script` the harness runs inside the guest, or `None`.
///
/// A four-line TOML reader rather than a dependency, and the same
/// shape as `profiles_disabling_overflow_checks`: section, key, value,
/// with the quoting stripped so `"script"` is `script`. What it has to
/// tell apart is `script` under `[setup]` from `guest_command` under
/// `[test]`, which is a different program run at a different time.
fn harness_setup_script(config: &str) -> Option<String> {
    let mut section = String::new();
    for raw in config.lines() {
        let line = raw.split('#').next().unwrap_or(raw).trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(|c| c == '"' || c == '\'');
        if section == "setup" && key == "script" {
            return Some(
                value
                    .trim()
                    .trim_matches(|c| c == '"' || c == '\'')
                    .to_string(),
            );
        }
    }
    None
}

/// The check-run names `.github-guard` requires before main takes a
/// merge.
///
/// git-config format, which is what github-guard reads: a `[checks]`
/// section and one `required = <name>` per line. A name may contain
/// spaces -- this repository required
/// `validate against xfs_db + in-kernel XFS driver` until #207 -- so
/// the value is taken whole and only a comma separates two of them.
/// Comments are stripped unless the value is quoted, which is the rule
/// the file's own header states.
fn required_checks(guard: &str) -> Vec<String> {
    let mut section = String::new();
    let mut out = Vec::new();
    for raw in guard.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            section = line
                .trim_matches(|c| c == '[' || c == ']')
                .trim()
                .to_string();
            continue;
        }
        let line = if line.starts_with('"') {
            line
        } else {
            line.split(['#', ';']).next().unwrap_or(line).trim()
        };
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if section != "checks" || key.trim() != "required" {
            continue;
        }
        out.extend(
            value
                .split(',')
                .map(|name| name.trim().trim_matches('"').trim().to_string())
                .filter(|name| !name.is_empty()),
        );
    }
    out
}

/// Whether a script compares a job's result against `success`.
///
/// Whitespace and quotes are removed first, so `!= "success"`,
/// `!='success'` and `!=success` are one shape -- the same
/// normalisation `profiles_disabling_overflow_checks` does for a TOML
/// key, after three spellings of one setting defeated it.
///
/// What this refuses is the plausible rewrite: comparing against
/// `failure` alone. A job that was skipped has the result `skipped`,
/// which is neither, so such a script reports green for a run in which
/// the job never happened. It is deliberately not a proof that the
/// comparison is the right way round -- that cannot be read out of
/// text -- and [`exits_non_zero`] is the other half of what can be.
fn compares_against_success(script: &str) -> bool {
    let compact: String = script
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
        .collect();
    compact.contains("=success")
}

/// Whether a script has any path that exits non-zero.
///
/// A check that prints a verdict and returns 0 gates nothing, which is
/// the other way `ci-ok` decays into a job that always reports green.
fn exits_non_zero(script: &str) -> bool {
    script.split("exit ").skip(1).any(|rest| {
        rest.trim_start()
            .starts_with(|c: char| c.is_ascii_digit() && c != '0')
    })
}

/// Whether a step takes the runner's own oracle tools away.
///
/// It has to name every tool in [`ORACLE_TOOLS`], look each one up on
/// the PATH, and move or remove what it finds. Each clause is one way
/// the step decays into a no-op that still reads like a precaution:
/// dropping a tool from the list leaves that one usable, and dropping
/// the `mv` leaves a loop that prints paths.
fn makes_the_runners_oracle_tools_unusable(run: &str) -> bool {
    let resolved = tools_resolved_with_command_v(run);
    let takes_them_away = shell_lines(run)
        .iter()
        .any(|line| line.contains("mv ") || line.contains("rm "));
    takes_them_away
        && ORACLE_TOOLS
            .iter()
            .all(|tool| resolved.iter().any(|found| found == tool))
}

/// The assumption every guard in this file rests on, asserted rather
/// than assumed.
///
/// Without it each of them is vacuous in the same way and none says
/// so: a workflow that stopped triggering on `pull_request` gates
/// nothing however its jobs are written, and `gating_jobs_running`
/// answers "none" for every task in the file -- which reads like six
/// separate findings about six separate jobs. This is the one that
/// names the cause (#153).
#[test]
fn the_ci_workflow_still_triggers_on_pull_request() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    if let Some(why) = not_a_pull_request_gate(&workflow) {
        panic!("{}: {why}", path.display());
    }
    assert!(
        !parse_workflow(&workflow).jobs.is_empty(),
        "control: {} has no jobs at all, so every scan over it is empty and every \
         `all()` over one is true",
        path.display()
    );
}

/// THE FIXTURES ARE WHAT THE INDEPENDENT mkfs.xfs BUILDS, and a gate
/// with no fixtures compares nothing.
///
/// What this hides when it is absent: `chore fixtures` is the only
/// thing in the gate that runs the kernel's own formatter, in the
/// harness VM, over every geometry the suite reads back. Drop the job
/// -- or give it an `if:`, or rename the task out from under it -- and
/// the tiers that read those images have nothing to read, so they
/// select nothing and pass, while `ci-ok` still goes green. That is
/// #208's shape, and rust-img-qcow2#97 is what it looks like once it
/// has happened: a probe that returned early on every runner, in every
/// job, on every push and pull request, while the test reported `ok`.
#[test]
fn the_pull_request_gate_still_builds_the_fixtures_in_the_harness_vm() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    let wf = parse_workflow(&workflow);
    assert!(
        !wf.jobs.is_empty(),
        "control: {} must parse into at least one job",
        path.display()
    );

    let jobs = gating_jobs_running(&wf, "fixtures");
    assert!(
        !jobs.is_empty(),
        "no job in {} runs `chore fixtures` IN A STEP WHOSE RESULT GATES A PULL \
         REQUEST, so nothing in the gate builds the kernel-made images the oracle and \
         kernel tiers read back. A job or step carrying `if:` or `continue-on-error:` \
         does not count: its result is not read. The tiers that consume the fixtures \
         do not fail without them in any way a required check can see -- they select \
         nothing, pass, and report green (#208, rust-img-qcow2#97). The jobs found \
         were {:?}.",
        path.display(),
        wf.jobs.iter().map(|j| &j.id).collect::<Vec<_>>()
    );
}

/// THE WHOLE SUITE, INCLUDING THE TIERS THIS CRATE DOES NOT GRADE
/// ITSELF.
///
/// What this hides when it is absent: `chore test` is what runs
/// `test:oracle` and `test:kernel` -- xfsprogs reading back what the
/// driver wrote, and Linux mounting it. A gate narrowed to
/// `chore test:unit` and `chore test:images` still runs hundreds of
/// tests and still reports green under the required name, while every
/// assertion that something OTHER than this crate agrees with it has
/// quietly stopped being made. That is the failure #208 was filed
/// for, and the one a required check cannot see.
#[test]
fn the_pull_request_gate_still_runs_the_whole_suite_against_the_oracles() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    let wf = parse_workflow(&workflow);
    assert!(
        !wf.jobs.is_empty(),
        "control: {} must parse into at least one job",
        path.display()
    );

    let jobs = gating_jobs_running(&wf, "test");
    assert!(
        !jobs.is_empty(),
        "no job in {} runs `chore test` IN A STEP WHOSE RESULT GATES A PULL REQUEST. \
         That task is the whole suite -- the oracle tier, where xfsprogs reads back \
         what the driver wrote, and the kernel tier, where Linux mounts it -- and it \
         is the only part of this gate that is not this crate grading itself. \
         `chore test:unit` and `chore test:images` are not it and must not stand in \
         for it. A job or step carrying `if:` or `continue-on-error:` does not count. \
         See #208. The jobs found were {:?}.",
        path.display(),
        wf.jobs.iter().map(|j| &j.id).collect::<Vec<_>>()
    );
}

/// THE ORACLE TOOLS ARE INSTALLED WHERE THE TESTS REACH THEM.
///
/// This is the direct successor of #208's "still installs its tool".
/// The tool moved: it used to be an `apt-get install xfsprogs` step in
/// the workflow, and it is now the guest's provisioning, because a
/// tool on the runner is a different version from the one every
/// developer has and the two answers are not comparable. So this is
/// where the guard has to look -- `fs-linux-test-harness.toml`'s
/// `[setup] script`, and what that script installs and proves present.
///
/// What it hides when it is absent: the tests never skip on a missing
/// tool, so a provisioning that stopped installing xfsprogs fails the
/// oracle tiers loudly -- ON A DAY WHEN SOMEBODY IS WATCHING. The
/// quiet version is the one that matters: drop a single tool from the
/// verification list and the tier that calls it is the only thing that
/// notices, months later, in a run nobody connects to this edit.
/// rust-img-qcow2#97 is the same defect with the tool present and the
/// call gone.
#[test]
fn the_oracle_tools_are_still_installed_in_the_guest_the_tests_reach() {
    let config_path = manifest_dir().join("fs-linux-test-harness.toml");
    let config = read_or_panic(&config_path);
    let named = harness_setup_script(&config).unwrap_or_else(|| {
        panic!(
            "{}: no `script` under `[setup]`, so nothing provisions the guest and the \
             oracle tools are wherever the base box happens to leave them",
            config_path.display()
        )
    });
    assert_eq!(
        named,
        "scripts/vm-setup.sh",
        "{}: the guest's provisioning is `{named}` now. That is where this \
         repository's oracle tools are installed and proved present, so the guard \
         below reads it; point it at the new file deliberately rather than leaving \
         the old one guarded and the new one unread.",
        config_path.display()
    );

    let script_path = manifest_dir().join(&named);
    let setup = read_or_panic(&script_path);
    let lines = shell_lines(&setup);
    assert!(
        !lines.is_empty(),
        "control: {} has no commands at all",
        script_path.display()
    );

    let installs_xfsprogs = lines
        .iter()
        .any(|line| line.contains("apt-get install") && names_word(line, "xfsprogs"));
    assert!(
        installs_xfsprogs,
        "{} no longer installs the `xfsprogs` package in the guest. Every oracle call \
         this suite makes is answered by that package -- mkfs.xfs builds the \
         fixtures, xfs_repair grades consistency, xfs_db reads metadata back -- and \
         the tests reach it in the VM and nowhere else (tests/test_contract.rs fails \
         the suite if one runs on the host). See #208.",
        script_path.display()
    );

    let resolved = tools_resolved_with_command_v(&setup);
    let unverified: Vec<&str> = ORACLE_TOOLS
        .iter()
        .copied()
        .filter(|tool| !resolved.iter().any(|found| found == tool))
        .collect();
    assert!(
        unverified.is_empty(),
        "{} installs the tools but no longer proves {unverified:?} present with \
         `command -v`. The provision is the only useful place to find a tool missing: \
         a test never skips on one, so the alternative is several hundred tests each \
         failing on the same absence, none of them naming it.",
        script_path.display()
    );
}

/// THE CLAIM TURNED INTO EVIDENCE.
///
/// The workflow says every xfsprogs call happens in the guest. The
/// step this guards is what makes that checkable rather than stated:
/// the runner's own copies are moved aside and replaced with stubs
/// that fail loudly, so a test reaching for a host tool turns the run
/// red with a message saying so instead of passing on a version no
/// other machine has.
///
/// It must be in the SAME JOB as the suite, which is why this asks for
/// the job rather than for a step anywhere in the file. What it hides
/// when it is absent: nothing, visibly. The suite goes green either
/// way -- that is exactly the problem, because it also goes green when
/// half the calls are being answered by Ubuntu's xfsprogs and the
/// fixtures were built by Debian's. That fork is #211 and #212, and it
/// is how the gate and a developer's run came to be graded by two
/// different oracles. See #208 for the general shape.
#[test]
fn the_job_that_runs_the_suite_still_makes_the_runners_own_xfsprogs_unusable() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    let wf = parse_workflow(&workflow);
    let jobs = gating_jobs_running(&wf, "test");
    assert!(
        !jobs.is_empty(),
        "control: {} must have a gating job running `chore test`, or this asserts \
         nothing about any job at all. See \
         the_pull_request_gate_still_runs_the_whole_suite_against_the_oracles.",
        path.display()
    );

    for job in &jobs {
        assert!(
            job.steps
                .iter()
                .any(|step| makes_the_runners_oracle_tools_unusable(&step.run)),
            "{}: the job `{}` runs the whole suite but no longer makes the runner's \
             own {:?} unusable. That step is what turns \"every oracle call happens in \
             the VM\" from a claim into evidence: without it a test that reached for a \
             host tool would be answered by whatever xfsprogs the runner image ships, \
             which is a different version from the one that built the fixtures and a \
             different one again from every developer's -- and the run would still be \
             green. See #208, and #211/#212 for what two oracles grading one suite \
             cost here.",
            path.display(),
            job.id,
            ORACLE_TOOLS
        );
    }
}

/// THE AGGREGATE IS THE REQUIRED CHECK, SO ITS WIRING IS THE GATE.
///
/// `ci-ok` is the only name branch protection requires (#207), which
/// buys a gate that survives a rename and costs one thing: everything
/// now depends on `needs:` being complete and on the script treating a
/// job that did not run as a failure. Both are invisible when wrong.
///
/// What this hides when it is absent: add a job and forget to wire it
/// into `needs:` and it gates nothing while looking exactly like the
/// others -- the `needs:` set is computed from the parsed workflow
/// here rather than listed, so that mistake is what fails. And a
/// script comparing results against `failure` alone passes a SKIPPED
/// job, which is the case the aggregate exists to catch: a job skipped
/// because its `needs:` failed reports neither success nor failure,
/// and #201 is this repository's own instance of a job that ran on
/// every pull request and gated nothing.
#[test]
fn the_aggregate_check_still_needs_every_job_and_fails_on_a_skipped_one() {
    let path = ci_yml();
    let workflow = read_or_panic(&path);
    let wf = parse_workflow(&workflow);
    assert!(
        !wf.jobs.is_empty(),
        "control: {} must parse into at least one job",
        path.display()
    );

    let aggregate = wf
        .jobs
        .iter()
        .find(|job| job.id == "ci-ok")
        .unwrap_or_else(|| {
            panic!(
                "{}: no job `ci-ok`. It is the one check .github-guard requires, so \
             without it branch protection requires a context nothing produces -- \
             which GitHub reports as permanently pending, with no failure to point \
             at. The jobs found were {:?}.",
                path.display(),
                wf.jobs.iter().map(|j| &j.id).collect::<Vec<_>>()
            )
        });

    let condition = aggregate.condition.as_deref().unwrap_or("");
    assert!(
        condition.contains("always()"),
        "{}: `ci-ok` carries `if: {condition}`. It must be `always()`: a job that \
         `needs:` every other one is SKIPPED when any of them fails, and a skipped \
         required check is not a failed one -- the merge button would go green on a \
         red run. This is the one place an `if:` is not only safe but required, which \
         is also why NON_GATING_KEYS must not be special-cased to let it gate.",
        path.display()
    );

    let mut expected: Vec<&str> = wf
        .jobs
        .iter()
        .map(|job| job.id.as_str())
        .filter(|id| *id != "ci-ok")
        .collect();
    expected.sort_unstable();
    assert!(
        !expected.is_empty(),
        "control: {} must contain a job besides `ci-ok`",
        path.display()
    );
    let mut needs: Vec<&str> = aggregate.needs.iter().map(String::as_str).collect();
    needs.sort_unstable();
    needs.dedup();
    assert_eq!(
        needs,
        expected,
        "{}: `ci-ok` does not `needs:` every other job in the file. The set is \
         computed from the workflow rather than listed here on purpose: adding a job \
         and forgetting to wire it in leaves it running on every pull request and \
         gating nothing, which is #201 exactly, and the aggregate is the only thing \
         that can notice.",
        path.display()
    );

    let script = aggregate
        .steps
        .iter()
        .map(|step| step.run.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !script.trim().is_empty(),
        "{}: `ci-ok` runs nothing, so it reports success whatever its needs did",
        path.display()
    );
    assert!(
        compares_against_success(&script),
        "{}: `ci-ok`'s script never compares a job's result against `success`. \
         Comparing against `failure` alone is the defect this job exists to prevent: \
         a job that was SKIPPED -- because it was cancelled, because its own `needs:` \
         failed, or because somebody gave it an `if:` -- has the result `skipped`, \
         which is not `failure`, and the aggregate would report green for a run in \
         which it never happened. Anything that is not `success` is a failure here.",
        path.display()
    );
    assert!(
        exits_non_zero(&script),
        "{}: `ci-ok`'s script never exits non-zero, so whatever it finds it reports \
         success. A check that prints a verdict and returns 0 is a check that gates \
         nothing.",
        path.display()
    );
}

/// THE LAST LINK: what branch protection actually requires.
///
/// The workflow can be perfect and gate nothing if `.github-guard`
/// requires a name it does not produce -- GitHub reads that as
/// permanently pending, with `enforce_admins` on and no failure to
/// point at -- or if it stops requiring `ci-ok`, which leaves every
/// job above advisory. Both had already happened in this
/// constellation: #201 is a job that ran on every pull request and
/// gated nothing, and this file's own header records the required
/// context that no job produced.
///
/// The names are checked against what the workflow PRODUCES, which is
/// a job's `name:` where it has one and its key where it does not --
/// the same rule GitHub uses for the check-run name.
#[test]
fn branch_protection_requires_the_aggregate_and_nothing_the_workflow_cannot_produce() {
    let guard_path = manifest_dir().join(".github-guard");
    let guard = read_or_panic(&guard_path);
    let required = required_checks(&guard);
    assert!(
        !required.is_empty(),
        "{}: no `required =` under `[checks]`, so nothing gates a merge at all and \
         every job in ci.yml is advisory",
        guard_path.display()
    );
    assert!(
        required.iter().any(|check| check == "ci-ok"),
        "{}: `ci-ok` is not required. It is the one always-run job that `needs:` \
         every other and fails on a failed, cancelled or SKIPPED one (#207); without \
         it required, the whole gate is advisory. Required now: {required:?}.",
        guard_path.display()
    );

    let workflow = read_or_panic(&ci_yml());
    let wf = parse_workflow(&workflow);
    let produced: Vec<String> = wf
        .jobs
        .iter()
        .map(|job| job.name.clone().unwrap_or_else(|| job.id.clone()))
        .collect();
    assert!(
        !produced.is_empty(),
        "control: ci.yml must produce at least one check-run name"
    );
    let missing: Vec<&String> = required
        .iter()
        .filter(|check| !produced.contains(check))
        .collect();
    assert!(
        missing.is_empty(),
        "{} requires {missing:?}, which ci.yml does not produce. GitHub reports a \
         required context nothing reports as permanently PENDING, not as failed: with \
         enforce_admins on there is nothing to point at and no way to merge, and the \
         usual fix is to drop the requirement, which silently removes a gate. The \
         names ci.yml produces are {produced:?}.",
        guard_path.display()
    );
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

    /// THE MESSAGE NAMES THE CAUSE (#153). A workflow that stopped
    /// triggering on pull requests is reported as that, with the
    /// triggers it has, and not as a missing debug step. The control is
    /// the gating shape, which has nothing to explain.
    #[test]
    fn a_workflow_off_pull_requests_is_reported_by_its_trigger() {
        assert_eq!(super::not_a_pull_request_gate(GATING), None, "control");
        for (trigger, names_target) in [
            ("pull_request_target", true),
            ("pull_request_review", false),
            ("push", false),
        ] {
            let yaml = GATING.replace("  pull_request:\n", &format!("  {trigger}:\n"));
            assert_ne!(yaml, GATING, "the mutation must actually apply");
            let why = super::not_a_pull_request_gate(&yaml)
                .unwrap_or_else(|| panic!("{trigger}: no reason given"));
            assert!(
                why.contains(&format!("{trigger:?}")) && why.contains("`on:` block"),
                "{trigger}: the message must name the trigger found and the on: block: {why}"
            );
            assert_eq!(
                why.contains("refused on purpose"),
                names_target,
                "{trigger}: only pull_request_target gets the refusal explained: {why}"
            );
        }
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

    /// `pull_request_target` ALONE IS NOT A PULL-REQUEST GATE, and
    /// `runs_on_pull_request` does not count it: it compares each
    /// parsed trigger name against `pull_request` and nothing else.
    ///
    /// Such a workflow runs in the base repository's context and checks
    /// out the base ref by default, so it may never build the
    /// contributor's code (#146). Replacing `pull_request:` with
    /// `pull_request_target:` is a defeat of the gate, and this test is
    /// what kills that mutation.
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
    /// This is why the check compares whole trigger names rather than
    /// matching a prefix.
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
                "`{trigger}` is not `pull_request`, so it must not be counted as one"
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

    /// The real `ci.yml` gates, read through the same walk the guards
    /// use. Distinct from the guards' own assertions: this one proves
    /// the PARSER copes with the real file's shape -- `uses:`/`with:`
    /// mappings, block scalars, comments between steps, an `if:` on a
    /// step in the middle of a gating job -- rather than only with the
    /// fixtures above.
    ///
    /// It asks for `chore test:unit` rather than for a `cargo test`,
    /// because that is what the real file now runs; the fixtures above
    /// keep the cargo half of the walk pinned.
    #[test]
    fn the_real_ci_yml_still_parses_into_a_gating_step() {
        let workflow = super::read_or_panic(&super::ci_yml());
        if let Some(why) = super::not_a_pull_request_gate(&workflow) {
            panic!("the real ci.yml: {why}");
        }
        assert!(
            !super::gating_chore_runs(&workflow, "test:unit").is_empty(),
            "the real ci.yml must parse into at least one gating step, or the guards \
             are passing on fixtures and failing on the file they exist to read"
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

    /// `-r` IS `--release`, IN EVERY SPELLING CLAP ACCEPTS (#159). Each of
    /// these compiles with overflow checks off and counted as the debug
    /// run; the controls beside them are not release and still count.
    #[test]
    fn the_short_release_flag_does_not_count_in_any_spelling() {
        for line in [
            "cargo test --locked -r --all-targets",
            "cargo test --locked -qr --all-targets",
            "cargo test --locked -rq --all-targets",
            "cargo test --locked -j4 -r",
            "cargo test --locked -j 4 -r --lib",
            "cargo test --locked --features x -r",
            "/usr/bin/cargo test --locked -r --lib",
            "cargo +1.95.0 test --locked -r --lib",
            "true&&cargo test --locked -r --lib",
            "cargo test --locked --lib 2>&1 -r",
            "cargo test --locked --features 'a;b' -r",
            "cargo test --locked --features \"a&&b\" -r",
            "cargo test --locked --features a\\;b -r",
            "cargo test --locked '-r'",
            "cargo test --locked \"-qr\"",
            "cargo test --locked \\-r",
            "RUSTFLAGS=-Dwarnings cargo test --locked -r",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line),
                Vec::<String>::new(),
                "{line} builds the release profile"
            );
        }
        for line in [
            "cargo test --locked --all-targets -- -r",
            "cargo test --locked --features r",
            "cargo test --locked -F r",
            "cargo test --locked -pr --lib",
            "cargo test --locked -j r --lib",
            "cargo test --locked --lib && rm -rf build",
            "cargo test --locked --lib; echo -r",
            "cargo test --locked --lib&&rm -rf build",
            "cargo test --locked --lib;echo -r",
            "cargo test --locked --lib|tee -r",
            "cargo test --locked -- '-r'",
            "cargo test --locked --lib && echo 'cargo test -r'",
            "cargo test --locked --lib && echo cargo test -r",
            "echo cargo test -r; cargo test --locked --lib",
        ] {
            assert_eq!(
                runs_with_overflow_checks(line).len(),
                1,
                "{line}: the r is a value or the harness's, and the run is debug"
            );
        }
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

/// A pull request gets CI whatever it is based on (#241).
///
/// `on: pull_request: branches: [main]` means a pull request based on
/// anything else runs no job in this workflow, and nothing says so: the
/// pull request simply has no checks. PR #229, stacked on another fix,
/// reported `CLEAN` with none at all beside siblings showing three —
/// and with `.github-guard` requiring only `ci-ok` (#207), such a pull
/// request has no `ci-ok` to wait for either.
#[test]
fn ci_runs_on_a_pull_request_against_any_base() {
    let path = manifest_dir().join(".github/workflows/ci.yml");
    let text = read_or_panic(&path);
    let filter: Vec<String> = {
        let mut out = Vec::new();
        let mut in_on = false;
        let mut in_pr = false;
        for line in text.lines() {
            let indent = line.len() - line.trim_start().len();
            let t = line.trim();
            if t.starts_with('#') {
                continue;
            }
            if indent == 0 && !t.is_empty() {
                in_on = t == "on:";
                in_pr = false;
                continue;
            }
            if !in_on {
                continue;
            }
            if indent == 2 && t.ends_with(':') {
                in_pr = t == "pull_request:";
                continue;
            }
            if in_pr && indent == 4 {
                if let Some(list) = t.strip_prefix("branches:") {
                    out.extend(
                        list.trim()
                            .trim_matches(['[', ']'].as_slice())
                            .split(',')
                            .map(|b| b.trim().trim_matches('\'').to_string())
                            .filter(|b| !b.is_empty()),
                    );
                }
            }
        }
        out
    };
    assert!(
        filter.is_empty(),
        "ci.yml runs on pull requests only against {filter:?}. One based on anything \
         else — a stacked pull request, which this repository uses — runs no job here \
         at all, and nothing says so: it simply has no checks, and no `ci-ok` for \
         protection to wait for."
    );
}

/// The #208 guards, held to the mutations they exist to refuse.
///
/// Each fixture is the shape the real files have with ONE thing
/// changed, and the changes are the ones #208 lists: the job renamed,
/// given an `if:`, given `continue-on-error:`, no longer installing
/// its tool, no longer running its suite. A control beside each proves
/// the unmodified shape IS counted, so none of them can pass for the
/// wrong reason -- which is the whole failure being guarded against,
/// one level up.
mod oracle_guard {
    use super::{
        chore_task_of, compares_against_success, exits_non_zero, gating_jobs_running,
        harness_setup_script, makes_the_runners_oracle_tools_unusable, parse_workflow,
        required_checks, runs_chore_task, shell_commands, task_commands,
        tools_resolved_with_command_v,
    };

    /// The shape `ci.yml` has: a gating job that builds the fixtures,
    /// a gating job that takes the runner's oracle tools away and runs
    /// the suite, and the always-run aggregate over both.
    const WORKFLOW: &str = r#"
on:
  pull_request:
    branches: [main]
jobs:
  fixtures:
    name: fixtures (harness VM)
    steps:
      - run: chore fixtures
  test:
    name: test (x86_64, oracles in the VM)
    needs: fixtures
    steps:
      - name: Make the runner's own xfsprogs unusable
        run: |
          for tool in mkfs.xfs xfs_db xfs_repair xfs_logprint xfs_io; do
            path="$(command -v "$tool" || true)"
            [ -n "$path" ] || continue
            sudo mv "$path" "$path.host-copy"
          done
      - run: chore test
  ci-ok:
    name: ci-ok
    if: always()
    needs: [fixtures, test]
    steps:
      - run: |
          bad="$(echo "$NEEDS" | jq -r '[to_entries[] | select(.value.result != "success")] | length')"
          [ "$bad" = 0 ] || exit 1
"#;

    const SUITE_STEP: &str = "      - run: chore test\n";

    fn gating(workflow: &str, task: &str) -> usize {
        gating_jobs_running(&parse_workflow(workflow), task).len()
    }

    /// The control. Without it every refusal below could be the
    /// fixture failing to parse rather than the mutation being caught.
    #[test]
    fn the_control_workflow_runs_both_oracle_tasks_in_gating_jobs() {
        assert_eq!(gating(WORKFLOW, "fixtures"), 1, "the fixtures job gates");
        assert_eq!(gating(WORKFLOW, "test"), 1, "the suite job gates");
    }

    /// THE RENAME. `chore test` and `chore test:unit` are different
    /// tasks, and the second runs none of the oracle tiers -- so a
    /// guard matching a prefix would report the gate cross-validating
    /// when it had been narrowed to the tier this crate grades itself.
    #[test]
    fn a_task_whose_name_merely_starts_the_same_is_a_different_task() {
        for (line, task) in [
            ("      - run: chore test:unit\n", "test"),
            ("      - run: chore testing\n", "test"),
            ("      - run: chore fixtures:clean\n", "fixtures"),
        ] {
            let yaml = WORKFLOW
                .replace(SUITE_STEP, line)
                .replace("      - run: chore fixtures\n", line);
            assert_ne!(yaml, WORKFLOW, "the mutation must actually apply");
            assert_eq!(
                gating(&yaml, task),
                0,
                "{line:?} does not run `chore {task}`, and counting it as one would \
                 let a narrowed gate pass"
            );
        }
    }

    /// THE `if:`, at either level. The job runs, or does not, and
    /// nothing reads the answer -- #143's defect, which left all 27
    /// guards green while the gate stopped gating.
    #[test]
    fn an_if_anywhere_above_the_oracle_step_stops_it_gating() {
        for yaml in [
            WORKFLOW.replace("  test:\n", "  test:\n    if: false\n"),
            WORKFLOW.replace(
                "  test:\n",
                "  test:\n    if: github.event_name == 'push'\n",
            ),
            WORKFLOW.replace(SUITE_STEP, &format!("{SUITE_STEP}        if: false\n")),
        ] {
            assert_ne!(yaml, WORKFLOW, "the mutation must actually apply");
            assert_eq!(
                gating(&yaml, "test"),
                0,
                "a job or step that may not run cannot be what cross-validates"
            );
            assert_eq!(
                gating(&yaml, "fixtures"),
                1,
                "control: the untouched fixtures job still gates, so the refusal is \
                 the mutation and not the fixture"
            );
        }
    }

    /// THE `continue-on-error:`. The job runs, the oracle disagrees,
    /// and the run is green anyway.
    #[test]
    fn continue_on_error_anywhere_above_the_oracle_step_stops_it_gating() {
        for yaml in [
            WORKFLOW.replace("  test:\n", "  test:\n    continue-on-error: true\n"),
            WORKFLOW.replace(
                SUITE_STEP,
                &format!("{SUITE_STEP}        continue-on-error: true\n"),
            ),
        ] {
            assert_ne!(yaml, WORKFLOW, "the mutation must actually apply");
            assert_eq!(
                gating(&yaml, "test"),
                0,
                "a result that is discarded is not a result the gate reads"
            );
        }
    }

    /// And the workflow-level version of the same: jobs that gate
    /// nothing because nothing they belong to runs on a pull request.
    #[test]
    fn a_workflow_off_pull_requests_gates_no_oracle_job() {
        let yaml = WORKFLOW.replace("  pull_request:\n", "  pull_request_review:\n");
        assert_ne!(yaml, WORKFLOW, "the mutation must actually apply");
        assert_eq!(gating(&yaml, "fixtures"), 0);
        assert_eq!(gating(&yaml, "test"), 0);
    }

    /// `chore` by name or by path, after the assignments the shell
    /// applies rather than runs, and the task is the first argument
    /// that is not a flag. The negative cases are the ones that matter:
    /// a task name after `--` belongs to the task, and in
    /// `echo chore test` the program is `echo`.
    #[test]
    fn the_chore_invocation_is_read_the_way_chore_reads_it() {
        for (command, task) in [
            ("chore test", Some("test")),
            ("/usr/local/bin/chore test", Some("test")),
            ("CHORE_VERSION=0.11.0 chore test", Some("test")),
            ("chore --verbose test", Some("test")),
            ("chore test -- --verbose", Some("test")),
            ("chore test:unit", Some("test:unit")),
            ("echo chore test", None),
            ("! chore vm:status", None),
            ("cargo test", None),
            ("chore -- test", None),
        ] {
            let words = shell_commands(command);
            let found = words.iter().find_map(|w| chore_task_of(w));
            assert_eq!(found, task, "{command:?}");
        }
    }

    /// A TASK NAMED IN A COMMENT IS NOT A TASK. `ci.yml` names every
    /// task it runs in the comment block at the top of the file, which
    /// is where the word survives the step's deletion.
    #[test]
    fn a_chore_task_named_in_a_comment_is_not_a_run() {
        for prose in [
            "#   test   `chore lint` and `chore test`\n",
            "    # the whole gate: chore test\n",
            "set -eu\necho 'ready'  # then chore test\n",
        ] {
            assert!(
                runs_chore_task(prose, "test").is_empty(),
                "{prose:?}: the prose describing the gate is not the gate"
            );
        }
        assert_eq!(
            runs_chore_task("set -eu\nchore test\n", "test").len(),
            1,
            "control: the command itself is, wherever in the block it sits"
        );
    }

    /// THE TOOL THAT STOPS BEING INSTALLED, and the subtler half: the
    /// tool that stops being CHECKED. `mkfs.xfs` and `xfs_db` are
    /// named all over both scripts -- in `apt-get install`, in a
    /// `-V` banner, in a tarball name -- so a text search finds them
    /// after the loop that proves them present has stopped covering
    /// them. Only the loop's own list counts, and only when its body
    /// does the lookup.
    #[test]
    fn a_tool_dropped_from_the_verification_is_no_longer_verified() {
        let script = "\
apt-get install -y -qq xfsprogs attr acl
mkfs.xfs -V
for tool in mkfs.xfs xfs_db xfs_repair; do
    command -v \"$tool\" || exit 1
done
";
        let found = tools_resolved_with_command_v(script);
        for tool in ["mkfs.xfs", "xfs_db", "xfs_repair"] {
            assert!(
                found.iter().any(|f| f == tool),
                "control: {tool} is checked"
            );
        }
        let dropped = script.replace("mkfs.xfs xfs_db xfs_repair", "xfs_db xfs_repair");
        assert_ne!(dropped, script, "the mutation must actually apply");
        assert!(
            !tools_resolved_with_command_v(&dropped)
                .iter()
                .any(|f| f == "mkfs.xfs"),
            "mkfs.xfs is still installed and still printed, and is no longer proved \
             present -- which is the half a text search cannot see"
        );
        let unchecked = script.replace(
            "    command -v \"$tool\" || exit 1\n",
            "    echo \"$tool\"\n",
        );
        assert_ne!(unchecked, script, "the mutation must actually apply");
        assert!(
            tools_resolved_with_command_v(&unchecked).is_empty(),
            "a loop over the tools that does not look them up proves nothing about them"
        );
    }

    /// THE STEP THAT MAKES THE RUNNER'S OWN TOOLS UNUSABLE, and the
    /// two ways it decays into a no-op that still reads like a
    /// precaution.
    #[test]
    fn the_poisoning_step_stops_counting_when_it_stops_working() {
        let step = &parse_workflow(WORKFLOW)
            .jobs
            .iter()
            .find(|job| job.id == "test")
            .expect("control: the fixture has a test job")
            .steps[0]
            .run
            .clone();
        assert!(
            makes_the_runners_oracle_tools_unusable(step),
            "control: the unmodified step takes every oracle tool away"
        );
        for mutation in [
            step.replace(" xfs_io;", ";"),
            step.replace("sudo mv \"$path\" \"$path.host-copy\"", "echo \"$path\""),
        ] {
            assert_ne!(&mutation, step, "the mutation must actually apply");
            assert!(
                !makes_the_runners_oracle_tools_unusable(&mutation),
                "a tool left usable, or a loop that only prints, is not evidence that \
                 the oracle calls happened in the VM"
            );
        }
    }

    /// The aggregate's wiring, read in both spellings `needs:` takes.
    #[test]
    fn the_aggregate_is_read_with_its_condition_and_its_needs() {
        let wf = parse_workflow(WORKFLOW);
        let aggregate = wf
            .jobs
            .iter()
            .find(|job| job.id == "ci-ok")
            .expect("control: the fixture has a ci-ok job");
        assert_eq!(aggregate.name.as_deref(), Some("ci-ok"));
        assert_eq!(aggregate.condition.as_deref(), Some("always()"));
        assert_eq!(aggregate.needs, ["fixtures", "test"]);

        let scalar = WORKFLOW.replace("    needs: [fixtures, test]\n", "    needs: test\n");
        assert_ne!(scalar, WORKFLOW, "the mutation must actually apply");
        let wf = parse_workflow(&scalar);
        let aggregate = wf.jobs.iter().find(|job| job.id == "ci-ok").unwrap();
        assert_eq!(
            aggregate.needs,
            ["test"],
            "a single job is a legal `needs:` and is one job, not a missing key -- \
             reading it as nothing would report every job as unwired"
        );
    }

    /// THE COMPARISON THE AGGREGATE EXISTS TO MAKE. A skipped job's
    /// result is `skipped`, so a script that only knows about
    /// `failure` reports green for a run in which the job never
    /// happened -- which is the case `ci-ok` was added for (#207).
    #[test]
    fn a_script_that_only_knows_about_failure_is_refused() {
        assert!(
            compares_against_success(r#"select(.value.result != "success")"#),
            "control: the real shape"
        );
        for spelling in [
            "[ \"$r\" != success ]",
            "[[ $r == \"success\" ]]",
            "select(.value.result!='success')",
        ] {
            assert!(
                compares_against_success(spelling),
                "{spelling}: the quoting is not the point"
            );
        }
        assert!(
            !compares_against_success(r#"select(.value.result == "failure")"#),
            "comparing against failure alone passes a job that was skipped"
        );
        assert!(
            !compares_against_success("echo \"the jobs succeeded\""),
            "a cheerful echo is not a comparison"
        );
    }

    /// And the other way it decays: a verdict printed and a zero
    /// returned.
    #[test]
    fn a_script_that_never_exits_non_zero_is_refused() {
        assert!(exits_non_zero("[ -z \"$bad\" ] || exit 1"), "control");
        assert!(
            exits_non_zero("exit 65"),
            "any non-zero status is a failure"
        );
        assert!(!exits_non_zero("echo \"::error::not green\"\nexit 0"));
        assert!(!exits_non_zero("echo \"not green\""));
    }

    /// The `[setup] script`, which is the one the guest is provisioned
    /// with. `[test] guest_command` is a different program at a
    /// different time, and confusing the two would guard a file that
    /// installs nothing.
    #[test]
    fn the_setup_script_is_read_from_the_right_section() {
        let config = "\
[project]
script = \"not-this-one.sh\"

[setup]
# Runs as root INSIDE the VM.
script = \"scripts/vm-setup.sh\"

[test]
guest_command = \"scripts/guest-suite.sh\"
";
        assert_eq!(
            harness_setup_script(config).as_deref(),
            Some("scripts/vm-setup.sh")
        );
        assert_eq!(
            harness_setup_script(&config.replace("script = \"scripts/vm-setup.sh\"\n", "")),
            None,
            "a `[setup]` with no script provisions nothing, and that is a finding"
        );
        assert_eq!(
            harness_setup_script(&config.replace(
                "script = \"scripts/vm-setup.sh\"",
                "'script' = 'scripts/vm-setup.sh'"
            ))
            .as_deref(),
            Some("scripts/vm-setup.sh"),
            "a quoted key is the same key -- the spelling that has defeated three \
             other comparisons in this repository"
        );
    }

    /// `.github-guard`, whose whole content is what a merge waits for.
    /// The names it carries have contained spaces and `+`, so the
    /// value is taken whole; a commented-out requirement is not a
    /// requirement, which is the state a file arrives in when somebody
    /// switches the gate off "for now".
    #[test]
    fn the_required_checks_are_read_from_the_checks_section_only() {
        let guard = "\
# ci-ok   ci.yml: the one always-run gate.
[other]
	required = not-a-check
[checks]
	required = ci-ok
	# required = validate against xfs_db + in-kernel XFS driver
";
        assert_eq!(required_checks(guard), ["ci-ok"]);
        let both = guard.replace("\trequired = ci-ok\n", "\trequired = ci-ok, test-darwin\n");
        assert_ne!(both, guard, "the mutation must actually apply");
        assert_eq!(required_checks(&both), ["ci-ok", "test-darwin"]);
        let spaced = guard.replace(
            "\trequired = ci-ok\n",
            "\trequired = validate against xfs_db + in-kernel XFS driver\n",
        );
        assert_eq!(
            required_checks(&spaced),
            ["validate against xfs_db + in-kernel XFS driver"],
            "a check-run name is whatever GitHub reports, spaces and all"
        );
        assert!(
            required_checks(&guard.replace("\trequired = ci-ok\n", "")).is_empty(),
            "nothing required is nothing gated, and it must be reported as that"
        );
    }

    /// `chores.yml`'s side: the commands of a task, and the three
    /// things that are not commands of it.
    #[test]
    fn a_tasks_commands_are_its_own() {
        let manifest = "\
version: \"3\"
tasks:
  test:unit:
    # `--release` is deliberately absent here.
    cmds:
      - 'EXPECT_OVERFLOW_CHECKS=1 cargo test --locked'
      - cmd: 'scripts/ci-test.sh --gate 290 tmp/logs/unit.log'
  test:native:
    cmds:
      - task: test:unit
      - 'scripts/ci-test.sh'
";
        assert_eq!(
            task_commands(manifest, "test:unit"),
            [
                "EXPECT_OVERFLOW_CHECKS=1 cargo test --locked",
                "scripts/ci-test.sh --gate 290 tmp/logs/unit.log"
            ],
            "a scalar and a `cmd:` mapping are both commands; the comment above them \
             is not one"
        );
        assert_eq!(
            task_commands(manifest, "test:native"),
            ["scripts/ci-test.sh"],
            "a `task:` reference is not this task's command -- crediting `test:native` \
             with `test:unit`'s cargo run would let the debug tier be defined \
             anywhere and found here"
        );
        assert!(
            task_commands(manifest, "unit").is_empty(),
            "a task that does not exist has no commands, and the guard's own \
             non-emptiness check is what turns that into a failure"
        );
    }
}
