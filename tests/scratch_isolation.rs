//! A volume a suite writes does not live beside the fixtures (#223).
//!
//! Several suites walk every `*.img` in `.vm-share` and grade this
//! driver against whatever they find. Others copy a fixture into that
//! same directory and write to the copy. Cargo runs test binaries in
//! parallel, so a scanner can pick up an image another suite is halfway
//! through writing and read the result as the driver's work — which is
//! an order-dependent failure whose only symptom is a number being off
//! by one, and #124 is what that looks like when it happens.
//!
//! So a scratch volume goes under `.vm-share/scratch/<suite>/` through
//! `common::scratch`, and nowhere else. The scanners use `read_dir`,
//! which does not recurse, so what is under there is invisible to them.
//!
//! Each suite writing its own guard is how the arrangement drifted in
//! the first place — seventeen copies of the same six lines, three of
//! them already placing the image somewhere safe and the rest not — so
//! what this pins is that there is one of them.

// Only `share` and the scratch helper are wanted here; the rest of the
// module is unused in this binary rather than unused.
#[allow(dead_code)]
mod common;

use common::{scratch, share};
use std::path::{Path, PathBuf};

fn tests_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests")
}

/// One guard, in `common`, rather than one per suite.
#[test]
fn no_suite_writes_its_own_scratch_guard() {
    let mut own = Vec::new();
    for entry in std::fs::read_dir(tests_dir()).expect("the tests directory") {
        let path = entry.expect("an entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a test file");
        if body.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with("//")
                && (l.starts_with("struct Scratch(") || l.starts_with("struct Scratch {"))
        }) {
            own.push(path.file_name().unwrap().to_string_lossy().to_string());
        }
    }
    assert!(
        own.is_empty(),
        "these suites keep their own scratch guard, so where their images go is their \
         own business and one of them will put the next one beside the fixtures: {own:?}"
    );
}

/// And what the shared one makes is out of a scanner's reach.
#[test]
fn a_scratch_volume_is_invisible_to_a_suite_that_scans_the_fixtures() {
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let suite = format!("scratch-isolation-{}", std::process::id());
    let volume = scratch::Volume::empty(&suite, "probe.img", 4096);
    assert!(
        volume.path().starts_with(share().join("scratch")),
        "a scratch volume should be under the scratch directory, and this one is at {}",
        volume.path().display()
    );
    assert_eq!(
        volume.guest(),
        format!("/share/scratch/{suite}/probe.img"),
        "the guest has to be told the same place the host wrote to"
    );

    // What a scanner sees: `read_dir` does not recurse, so the probe is
    // not among the images it would grade.
    let listed: Vec<String> = std::fs::read_dir(share())
        .expect("the fixture directory")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        !listed.iter().any(|n| n == "probe.img"),
        "a scratch volume turned up beside the fixtures: {listed:?}"
    );
}
