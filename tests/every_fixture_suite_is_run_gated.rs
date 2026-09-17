//! Every suite that reads `.vm-share` is run by `scripts/ci-test.sh` in
//! some workflow (#104, #105, #106, #108).
//!
//! A fixture-gated suite skips and passes when its fixtures are absent.
//! The `test` job builds none, so a suite no workflow hands to
//! `ci-test.sh` -- which fails a job on a skip -- passes on every run
//! without reading an image. Fifteen suites were in that state, among
//! them the C ABI's and the directory parser's oracles, and nothing
//! said so: each was noticed by reading the workflow by hand.
//!
//! This reads the workflows the same way: a suite counts as run when a
//! `run:` line says `ci-test.sh --test <suite>`, or names it in a
//! `for suite in ...` list whose body runs `ci-test.sh --test "$suite"`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Suites that read `.vm-share` but whose fixtures no workflow builds
/// yet, each with the issue that tracks it. An entry here that is run,
/// or no longer reads `.vm-share`, fails the test, so the list cannot
/// outlive its reason.
const NOT_YET_RUN: &[(&str, &str)] = &[];

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The suites `workflow` hands to `ci-test.sh`.
fn suites_run_gated(workflow: &str) -> BTreeSet<String> {
    // Continuation lines joined, so a loop's list is one line.
    let joined = workflow.replace("\\\n", " ");
    let lines: Vec<&str> = joined.lines().map(str::trim).collect();
    let mut out = BTreeSet::new();
    for (at, line) in lines.iter().enumerate() {
        if line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace().peekable();
        while let Some(word) = words.next() {
            if word.ends_with("ci-test.sh") && words.peek() == Some(&"--test") {
                words.next();
                if let Some(name) = words.next() {
                    if !name.starts_with('"') && !name.starts_with('$') {
                        out.insert(name.to_string());
                    }
                }
            }
        }
        if let Some(list) = line
            .strip_prefix("for suite in ")
            .and_then(|rest| rest.strip_suffix("; do"))
        {
            let body_runs_each = lines[at + 1..]
                .iter()
                .take_while(|l| **l != "done")
                .any(|l| l.contains("ci-test.sh --test \"$suite\""));
            if body_runs_each {
                out.extend(list.split_whitespace().map(str::to_string));
            }
        }
    }
    out
}

/// The test suites whose source reads `.vm-share`: by name, or through
/// `common::share()`.
fn fixture_gated_suites(tests: &Path) -> BTreeSet<String> {
    std::fs::read_dir(tests)
        .expect("read tests/")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .filter(|path| {
            let source = std::fs::read_to_string(path).expect("read suite");
            source.contains("\".vm-share\"")
                || (source.contains("mod common;") && source.contains("share()"))
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        // This file names `.vm-share` without reading it.
        .filter(|name| name != env!("CARGO_CRATE_NAME"))
        .collect()
}

#[test]
fn every_suite_that_reads_vm_share_is_run_where_a_skip_fails() {
    let root = manifest_dir();
    let mut run = BTreeSet::new();
    let workflows = root.join(".github").join("workflows");
    for entry in std::fs::read_dir(&workflows).expect("read .github/workflows") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "yml" || e == "yaml") {
            run.extend(suites_run_gated(&std::fs::read_to_string(&path).unwrap()));
        }
    }
    let gated = fixture_gated_suites(&root.join("tests"));
    assert!(
        gated.len() > 20 && run.len() > 20,
        "the scan found {} gated suites and {} run ones; it is reading the wrong thing",
        gated.len(),
        run.len()
    );

    let excused: BTreeSet<&str> = NOT_YET_RUN.iter().map(|(name, _)| *name).collect();
    let unrun: Vec<&String> = gated
        .iter()
        .filter(|s| !run.contains(*s) && !excused.contains(s.as_str()))
        .collect();
    assert!(
        unrun.is_empty(),
        "these suites read .vm-share and no workflow runs them through \
         scripts/ci-test.sh, so they skip and pass on every run: {unrun:?}"
    );
    for (name, why) in NOT_YET_RUN {
        assert!(
            gated.contains(*name) && !run.contains(*name),
            "{name} is excused ({why}) but is now run, or no longer reads .vm-share; \
             remove it from NOT_YET_RUN"
        );
    }
}

#[test]
fn the_scan_reads_a_loop_only_when_its_body_is_gated() {
    let gated = "        run: |\n          for suite in a_oracle \\\n                       b_oracle; do\n            ./scripts/ci-test.sh --test \"$suite\"\n          done\n      - run: ./scripts/ci-test.sh --test c_oracle\n";
    assert_eq!(
        suites_run_gated(gated),
        ["a_oracle", "b_oracle", "c_oracle"]
            .into_iter()
            .map(String::from)
            .collect()
    );
    let ungated = "          for suite in a_oracle b_oracle; do\n            cargo test --test \"$suite\"\n          done\n          # ./scripts/ci-test.sh --test c_oracle\n";
    assert!(
        suites_run_gated(ungated).is_empty(),
        "a plain cargo test loop, or a commented line, does not fail on a skip"
    );
}
