//! `fsck.xfs --dry-run` plans a repair against what the reference says,
//! and writes nothing (#375).
//!
//! Each volume here was made by `mkfs.xfs` and the kernel in the harness
//! guest, and each damage by `xfs_db -x`, so the planner is graded on
//! volumes it did not make. For each one:
//!
//! - **The image is unchanged.** Its SHA-256 is taken before the dry run
//!   and after it, and they must be equal. A plan that wrote would be a
//!   repair nobody asked for.
//! - **The plan is deterministic.** The dry run is made twice and the two
//!   `plan` objects must be byte for byte the same.
//! - **The reference agrees about the volume.** A volume planned as clean
//!   is one `xfs_repair -n` calls clean; a damage left unplanned is one it
//!   finds.
//!
//! And each refusal is the one the volume calls for: a feature the
//! planner does not reason about (`rmapbt`, on the reflink fixture the
//! reference calls clean), a log the kernel left unreplayed, two files
//! claiming one block, and a target another holder has locked.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, kernel_run, oracle, scratch, share};
use sha2::{Digest, Sha256};
use std::io::Read;

const SUITE: &str = "cli_repair_plan_oracle";

/// `fsck.xfs --dry-run` on `image`: its exit status and its JSON report.
fn dry_run(image: &str) -> (Option<i32>, String) {
    let out = tool("fsck.xfs")
        .args(["--dry-run", image])
        .output()
        .expect("spawn fsck.xfs");
    (out.status.code(), stdout(&out))
}

/// The report's `plan` object, as text with the whitespace between
/// tokens taken out, so two runs can be compared byte for byte and a case
/// can match on a fragment whatever the indentation.
fn plan_of(report: &str) -> String {
    let at = report
        .find("\"plan\"")
        .unwrap_or_else(|| panic!("no plan in the report:\n{report}"));
    let start = at + report[at..].find('{').expect("the plan is an object");
    let mut depth = 0usize;
    for (i, ch) in report[start..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return compact(&report[start..=start + i]);
                }
            }
            _ => {}
        }
    }
    panic!("the plan object never closes:\n{report}")
}

/// `json` without the whitespace outside its strings.
fn compact(json: &str) -> String {
    let mut out = String::with_capacity(json.len());
    let (mut in_string, mut escaped) = (false, false);
    for ch in json.chars() {
        if in_string {
            out.push(ch);
            match (escaped, ch) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
        } else if ch == '"' {
            in_string = true;
            out.push(ch);
        } else if !ch.is_whitespace() {
            out.push(ch);
        }
    }
    out
}

/// SHA-256 of the whole image.
fn image_hash(path: &std::path::Path) -> String {
    let mut file =
        std::fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf).expect("read the image");
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    format!("{:x}", hasher.finalize())
}

/// `xfs_repair -n` on `image`: whether it called the volume clean.
fn reference_clean(image: &str) -> (bool, String) {
    let out = oracle("xfs_repair").args(["-n", image]).output();
    let report = out.repair_report();
    assert!(
        !common::repair::was_blind(&report),
        "xfs_repair -n declined to look at {image}: its log holds records\n{report}"
    );
    (out.ok(), report)
}

/// Dry-run `volume` twice, require the image unchanged and the two plans
/// identical, and return the exit status and the plan.
fn planned_twice(volume: &std::path::Path, what: &str) -> (Option<i32>, String) {
    let image = volume.to_str().unwrap();
    let before = image_hash(volume);
    let (first_code, first) = dry_run(image);
    let (second_code, second) = dry_run(image);
    assert_eq!(
        image_hash(volume),
        before,
        "{what}: the dry run changed the image"
    );
    assert_eq!(first_code, second_code, "{what}: the exit status changed");
    let plan = plan_of(&first);
    assert_eq!(
        plan,
        plan_of(&second),
        "{what}: two dry runs of one volume planned differently"
    );
    (first_code, plan)
}

fn copy(source: &str, tag: &str) -> scratch::Volume {
    scratch::Volume::copy_of(
        SUITE,
        &fixture(source),
        &format!("{}-{tag}.img", std::process::id()),
    )
}

#[test]
fn a_volume_the_reference_calls_clean_plans_nothing() {
    for source in ["xfs-default.img", "xfs-1k.img", "xfsdata-default.img"] {
        let volume = copy(source, "clean");
        let (clean, said) = reference_clean(volume.path().to_str().unwrap());
        assert!(clean, "{source}: xfs_repair -n calls it damaged:\n{said}");
        let (code, plan) = planned_twice(volume.path(), source);
        assert_eq!(code, Some(0), "{source}: {plan}");
        assert_eq!(
            plan, r#"{"status":"ready","changes":[],"refusals":[],"unplanned":[]}"#,
            "{source}"
        );
    }
}

#[test]
fn damage_no_rule_repairs_is_planned_as_left() {
    let volume = copy("xfsdata-default.img", "freeblks");
    let edit = oracle("xfs_db")
        .args(["-x", "-c", "agf 0", "-c", "write -d freeblks 1"])
        .arg(volume.path())
        .output();
    assert!(
        edit.ok(),
        "xfs_db could not damage the AGF:\n{}",
        edit.stderr
    );
    let (clean, said) = reference_clean(volume.path().to_str().unwrap());
    assert!(!clean, "xfs_repair -n does not see the damage:\n{said}");
    let (code, plan) = planned_twice(volume.path(), "agf-freeblks");
    assert_eq!(code, Some(4), "{plan}");
    assert!(
        plan.starts_with(r#"{"status":"ready","changes":[],"refusals":[]"#),
        "{plan}"
    );
    assert!(
        plan.contains(r#""code":"counter.agf.freeblks""#),
        "the damage is not listed as unplanned: {plan}"
    );
}

#[test]
fn two_files_claiming_one_block_are_refused_as_ambiguous() {
    let source = fixture("xfsdata-default.img");
    let listing = stdout(&ok(tool("fs.xfs").args([
        source.to_str().unwrap(),
        "ls",
        "/",
    ])));
    let inode = |name: &str| {
        let at = listing
            .find(&format!("\"name\": \"{name}\""))
            .unwrap_or_else(|| panic!("no {name} in the data fixture:\n{listing}"));
        json_field(&listing[at..], "inode")
    };
    let (medium, large) = (inode("medium.bin"), inode("large.bin"));
    let shown = oracle("xfs_db")
        .args([
            "-r",
            "-c",
            &format!("inode {large}"),
            "-c",
            "p u3.bmx[0].startblock",
        ])
        .arg(&source)
        .output();
    let shared = shown
        .stdout
        .split('=')
        .nth(1)
        .unwrap_or_else(|| panic!("no startblock for large.bin:\n{}", shown.stdout))
        .trim()
        .to_string();
    let volume = copy("xfsdata-default.img", "cross-link");
    let edit = oracle("xfs_db")
        .args([
            "-x",
            "-c",
            &format!("inode {medium}"),
            "-c",
            &format!("write -d u3.bmx[0].startblock {shared}"),
        ])
        .arg(volume.path())
        .output();
    assert!(edit.ok(), "xfs_db could not cross-link:\n{}", edit.stderr);
    let (clean, said) = reference_clean(volume.path().to_str().unwrap());
    assert!(!clean, "xfs_repair -n does not see the cross-link:\n{said}");
    let (code, plan) = planned_twice(volume.path(), "cross-link");
    assert_eq!(code, Some(4), "{plan}");
    assert!(
        plan.starts_with(r#"{"status":"refused","changes":[]"#),
        "{plan}"
    );
    assert!(plan.contains(r#""code":"repair.ambiguous""#), "{plan}");
}

#[test]
fn a_feature_the_planner_does_not_reason_about_is_refused_on_a_clean_volume() {
    let volume = copy("xfs-reflink.img", "rmapbt");
    let (clean, said) = reference_clean(volume.path().to_str().unwrap());
    assert!(
        clean,
        "xfs_repair -n calls the reflink fixture damaged:\n{said}"
    );
    let (code, plan) = planned_twice(volume.path(), "rmapbt");
    assert_eq!(code, Some(0), "a refusal to plan is not damage: {plan}");
    assert!(
        plan.starts_with(r#"{"status":"refused","changes":[]"#),
        "{plan}"
    );
    assert!(
        plan.contains(r#""code":"repair.feature""#)
            && plan.contains(r#""field":"sb_features_ro_compat""#),
        "{plan}"
    );
}

#[test]
fn a_log_the_kernel_left_unreplayed_is_refused() {
    assert!(
        share().is_dir(),
        "no {}: `chore fixtures` makes it, and the guest this suite needs",
        share().display()
    );
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-dirty.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = volume.guest();
    // THE CRASH IS `shutdown -f`, as in tests/dirty_log_mount.rs: the log
    // is flushed and the filesystem stopped before anything checkpoints.
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        sync
        mkdir "$m/unflushed"
        for i in $(seq 0 63); do : > "$m/unflushed/f_$i"; done
        xfs_io -x -c 'shutdown -f' "$m" && echo SHUTDOWN_OK
        umount "$m" || umount -l "$m" || echo UMOUNT_FAILED
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK") && built.contains("SHUTDOWN_OK"),
        "building the dirty volume failed:\n{built}"
    );
    assert!(
        !built.contains("UMOUNT_FAILED"),
        "the shut-down volume was never unmounted, so the guest may still hold it:\n{built}"
    );
    let (code, plan) = planned_twice(volume.path(), "dirty log");
    assert_ne!(code, Some(8), "{plan}");
    assert!(
        plan.starts_with(r#"{"status":"refused","changes":[]"#),
        "{plan}"
    );
    assert!(plan.contains(r#""code":"repair.log-dirty""#), "{plan}");
}

#[test]
fn a_target_another_holder_has_locked_is_refused_and_not_read() {
    let volume = copy("xfs-default.img", "locked");
    let held = std::fs::File::open(volume.path()).expect("open the copy");
    held.lock().expect("the test takes the lock first");
    let before = image_hash(volume.path());
    let (code, report) = dry_run(volume.path().to_str().unwrap());
    assert_eq!(code, Some(8), "{report}");
    assert!(compact(&report).contains(r#""scan":"none""#), "{report}");
    let plan = plan_of(&report);
    assert!(
        plan.starts_with(r#"{"status":"refused","changes":[]"#),
        "{plan}"
    );
    assert!(plan.contains(r#""code":"repair.not-exclusive""#), "{plan}");
    drop(held);
    assert_eq!(
        image_hash(volume.path()),
        before,
        "the refused dry run changed the image"
    );
}
