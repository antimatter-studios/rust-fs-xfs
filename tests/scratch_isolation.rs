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
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volume
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volume beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
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

    // A volume leaves its suite's directory behind (#319); this suite is
    // named for the process, so it removes its own.
    let dir = volume.path().parent().expect("a parent").to_path_buf();
    drop(volume);
    let _ = std::fs::remove_dir(&dir);
}

/// A test that finishes does not take the scratch directory away from a
/// test in the same suite that is about to copy into it (#319).
///
/// `Volume::copy_of` is two steps: make `.vm-share/scratch/<suite>/`,
/// then copy the fixture into it. Cargo runs a suite's tests on several
/// threads, and a volume dropped between those two steps used to remove
/// the directory whenever it was the last file in it — so the copy
/// failed with ENOENT, and the panic named the *fixture* as the thing
/// missing. That is how `create_replay_oracle` went red in the guest on
/// some runs and not others, with every fixture present in the artifact.
///
/// The two steps are spelled out here with the other test's drop between
/// them, which is the interleaving that failed, made deterministic.
#[test]
fn a_finished_volume_leaves_the_suite_directory_to_the_tests_still_running() {
    assert!(
        share().is_dir(),
        "{} is not there: `chore fixtures` builds the set and makes the directory. \
         Tests never skip on a missing fixture.",
        share().display()
    );
    let suite = format!("scratch-shared-{}", std::process::id());
    let source = scratch::Volume::empty(&format!("{suite}-source"), "source.img", 4096);

    // One test has its directory and is about to copy into it...
    let waiting = scratch::dir(&suite);
    // ...when another test in the same suite finishes with its volume.
    drop(scratch::Volume::empty(&suite, "finished.img", 4096));

    let target = waiting.join("copy.img");
    let copied = std::fs::copy(source.path(), &target);
    let _ = std::fs::remove_file(&target);
    let _ = std::fs::remove_dir(&waiting);
    let source_dir = source.path().parent().expect("a parent").to_path_buf();
    drop(source);
    let _ = std::fs::remove_dir(&source_dir);
    copied.unwrap_or_else(|e| {
        panic!(
            "copying into {} after another test in the suite dropped its volume: {e}. \
             The drop removed the directory out from under a test still using it.",
            waiting.display()
        )
    });
}
