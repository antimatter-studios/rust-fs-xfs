//! Every suite that reads `.vm-share`, runs an oracle tool or asks the
//! kernel is in a tier, and every tier runs through `scripts/ci-test.sh`
//! (#104, #105, #106, #108).
//!
//! A fixture-gated suite that nothing runs passes on every run without
//! reading an image. Fifteen suites were in that state, among them the C
//! ABI's and the directory parser's oracles, and nothing said so: each
//! was noticed by reading the workflow by hand.
//!
//! # What changed, and why this test did too
//!
//! The list used to live in `.github/workflows/ci.yml` — thirty-two
//! suite names in a `for suite in ...` loop — and this test read the
//! workflows for the literal shape `ci-test.sh --test <suite>` to decide
//! which suites were covered. A suite was in the gate because somebody
//! had typed its name there, and this test existed to notice when
//! somebody had not.
//!
//! The tiers replace the list. `scripts/test-targets.sh` derives each
//! tier from what a test file CALLS, so a new suite is in a tier the
//! moment it reaches a fixture, a tool or the kernel, and there is
//! nothing to forget. What is left to check is that the derivation
//! covers everything and that every tier is still gated:
//!
//!   1. every suite that reaches a fixture, a tool or the kernel is
//!      selected by exactly one of `images`, `oracle`, `kernel`, and
//!      never by `unit`;
//!   2. every suite is selected by exactly one tier, so `chore test`
//!      runs each one and none twice within a tier;
//!   3. every tier runs through `scripts/ci-test.sh`, which fails a run
//!      that printed a skip or executed fewer tests than its floor.
//!
//! The third is the one that carries the old guarantee: a tier that
//! stopped going through the script would take its whole set of suites
//! out of the skip gate at once, which is the same defect as a suite
//! left out of the list, only bigger.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

const TIERS: [&str; 5] = ["unit", "images", "stress", "oracle", "kernel"];

/// The suites `scripts/test-targets.sh` puts in `tier`.
fn tier_members(tier: &str) -> BTreeSet<String> {
    let script = manifest_dir().join("scripts").join("test-targets.sh");
    let out = Command::new("bash")
        .arg(&script)
        .arg(tier)
        .current_dir(manifest_dir())
        .output()
        .unwrap_or_else(|e| panic!("cannot run {}: {e}", script.display()));
    assert!(
        out.status.success(),
        "{} {tier} exited {:?}:\n{}",
        script.display(),
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    // `--test NAME` pairs, plus the bare `--lib` / `--bins` the unit
    // tier adds.
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut words = text.split_whitespace();
    let mut names = BTreeSet::new();
    while let Some(word) = words.next() {
        if word == "--test" {
            names.insert(words.next().expect("--test with no name").to_string());
        }
    }
    names
}

/// Every integration suite in `tests/`.
fn all_suites() -> BTreeSet<String> {
    std::fs::read_dir(manifest_dir().join("tests"))
        .expect("read tests/")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// The suites whose source reaches a fixture, an oracle tool or the
/// kernel — read here the same way `scripts/test-targets.sh` reads them,
/// so that a suite the script's patterns miss fails this test rather
/// than landing silently in the unit tier.
fn suites_that_need_the_guest_or_a_fixture(tests: &Path) -> BTreeSet<String> {
    // THE SAME LIST scripts/test-targets.sh matches on, and it has to
    // stay the same list. When the fixture-absence skips were removed,
    // three suites stopped naming `.vm-share` at all — the lookup each
    // had written by hand became one call to the shared helper — and a
    // scan that knew only the old spelling let them fall into the unit
    // tier, which CI runs on a runner with no fixtures.
    const REACHES: [&str; 9] = [
        "fixture(",
        "fixtures_matching(",
        ".vm-share",
        "share()",
        "oracle(",
        "assert_xfs_repair_clean(",
        "kernel_run(",
        "guest_script(",
        "parent_oracle(",
    ];
    std::fs::read_dir(tests)
        .expect("read tests/")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|e| e == "rs"))
        .filter(|path| {
            let source = std::fs::read_to_string(path).expect("read suite");
            // COMMENT LINES DROPPED FIRST, exactly as
            // scripts/test-targets.sh drops them. A suite that explains
            // the patterns in prose — this one, and
            // tests/test_contract.rs — is not a suite that uses them,
            // and a scan that cannot tell the two apart would report
            // every meta-test as needing a VM it never speaks to.
            let code: String = source
                .lines()
                .filter(|line| !line.trim_start().starts_with("//"))
                .collect::<Vec<_>>()
                .join("\n");
            REACHES.iter().any(|needle| code.contains(needle))
        })
        .map(|path| path.file_stem().unwrap().to_string_lossy().into_owned())
        // This file names those patterns in a constant without calling
        // any of them.
        .filter(|name| name != env!("CARGO_CRATE_NAME"))
        .collect()
}

#[test]
fn every_suite_is_in_exactly_one_tier() {
    let members: BTreeMap<&str, BTreeSet<String>> =
        TIERS.iter().map(|t| (*t, tier_members(t))).collect();
    let suites = all_suites();
    assert!(
        suites.len() > 40,
        "the scan found {} suites; it is reading the wrong place",
        suites.len()
    );

    let mut wrong = Vec::new();
    for suite in &suites {
        let in_tiers: Vec<&str> = TIERS
            .iter()
            .copied()
            .filter(|t| members[t].contains(suite))
            .collect();
        if in_tiers.len() != 1 {
            wrong.push(format!("{suite}: {in_tiers:?}"));
        }
    }
    assert!(
        wrong.is_empty(),
        "these suites are in no tier, or in more than one, so `chore test` either \
         never runs them or runs them twice. The tiers come from \
         scripts/test-targets.sh and must partition tests/*.rs:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn no_suite_that_needs_the_guest_or_a_fixture_is_in_the_unit_tier() {
    let unit = tier_members("unit");
    let needy = suites_that_need_the_guest_or_a_fixture(&manifest_dir().join("tests"));
    assert!(
        needy.len() > 20,
        "the scan found {} suites that reach a fixture, a tool or the kernel; \
         it is reading the wrong thing",
        needy.len()
    );
    let misfiled: Vec<&String> = needy.iter().filter(|s| unit.contains(*s)).collect();
    assert!(
        misfiled.is_empty(),
        "these reach a fixture, an oracle tool or the kernel and are in the UNIT \
         tier, which CI runs on a runner with no fixtures and no VM. They would \
         fail there, or — worse, if they learned to skip — pass having checked \
         nothing. scripts/test-targets.sh classifies by what a test calls; a suite \
         here means its call is one the patterns do not recognise: {misfiled:?}"
    );
}

/// Every suite `scripts/test-targets.sh` names in STRESS_SUITES really
/// does read the fsstress/fsx corpus.
///
/// That list is the one place in the classifier that is a list rather
/// than a rule, because the corpus is the one fixture set `chore
/// fixtures` does not build and "needs it" cannot be told from "reads it
/// when it is there" by grepping for a name: tests/stress_oracle.rs
/// cannot run without it, tests/buf_item_oracle.rs walks whatever images
/// exist and names the same files among them. A list can grow a suite
/// that has nothing to do with the corpus, and that suite would then sit
/// in a tier `chore test` never runs — coverage lost with nothing saying
/// so. This is what stops that.
#[test]
fn the_stress_tier_only_holds_suites_that_read_the_corpus() {
    let members = tier_members("stress");
    assert!(
        !members.is_empty(),
        "scripts/test-targets.sh puts nothing in the stress tier, so \
         .github/workflows/stress.yml runs nothing against the corpus it \
         spends tens of minutes building"
    );
    for suite in &members {
        let path = manifest_dir().join("tests").join(format!("{suite}.rs"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            source.contains("xfsstress-"),
            "{suite} is in STRESS_SUITES but names no xfsstress- image, so `chore \
             test` does not run it and the weekly stress workflow has no reason to"
        );
    }
}

#[test]
fn every_tier_runs_through_the_skip_and_floor_gate() {
    let chores = std::fs::read_to_string(manifest_dir().join("chores.yml")).expect("read chores");
    // One command per tier: the line that runs it. A tier that stopped
    // going through scripts/ci-test.sh would take its whole set of
    // suites out of the skip gate at once.
    let mut ungated = Vec::new();
    for tier in ["images", "stress", "oracle", "kernel"] {
        let marker = format!("$(scripts/test-targets.sh {tier})");
        let line = chores
            .lines()
            .find(|line| line.contains(&marker))
            .unwrap_or_else(|| panic!("chores.yml does not run the {tier} tier at all"));
        if !line.contains("scripts/ci-test.sh") {
            ungated.push(format!("{tier}: {}", line.trim()));
        }
    }
    // The unit tier cannot use ci-test.sh's run mode — that mode pins
    // `--release` and the unit tier is the debug one — so it hands the
    // log tier.sh wrote to `ci-test.sh --gate`, which applies the same
    // pattern and the same floor.
    let unit: Vec<&str> = chores
        .lines()
        .filter(|line| line.contains("scripts/ci-test.sh --gate"))
        .collect();
    assert_eq!(
        unit.len(),
        1,
        "chores.yml must gate the debug unit tier with `ci-test.sh --gate`, which \
         applies the skip pattern and the executed-test floor to the log tier.sh \
         already wrote. Found: {unit:?}"
    );
    assert!(
        ungated.is_empty(),
        "these tiers do not go through scripts/ci-test.sh, so a suite in them can \
         print a skip, or execute nothing at all, and the run still reports \
         green:\n{}",
        ungated.join("\n")
    );
}
