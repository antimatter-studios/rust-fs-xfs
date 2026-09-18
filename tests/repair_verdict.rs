//! An `xfs_repair` report that says it ignored the log is not a verdict
//! (#124).
//!
//! Every write-path oracle ends the same way: the kernel replays what
//! this driver logged, and `xfs_repair -n` is asked whether the result is
//! sound. `-n` does not replay, and on an image whose log still holds
//! records the tool says so itself:
//!
//! ```text
//! ALERT: The filesystem has valuable metadata changes in a log which is being
//! ignored because the -n option was used.  Expect spurious inconsistencies
//! which may be resolved by first mounting the filesystem to replay the log.
//! ```
//!
//! A report carrying that line is not evidence either way. Read as a
//! failure it is a driver blamed for a tool's refusal to look — which is
//! what #124 was filed about, `sb_fdblocks 84960, counted 84959`. Read
//! as a pass, which is what a zero return code beside that ALERT
//! produces, it is a suite reporting green while checking nothing.
//!
//! So no oracle reads the return code by itself. They all go through
//! `common::assert_repair_agreed`, which refuses a blind report in
//! either direction and says which of the two it is.

mod common;

use common::{kernel_run, repair, share};

/// The oracles this rule is about: every test that asks `xfs_repair` for
/// a verdict has to read the answer through the shared check.
#[test]
fn every_oracle_reads_its_repair_report_through_the_check() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut unchecked = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("the tests directory") {
        let path = entry.expect("a directory entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if name == "repair_verdict.rs" {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a test file");
        // Only the tests that actually run the tool. A line of prose
        // naming it — every oracle's module header does — is not a run,
        // so comments do not count.
        let runs_it = body.lines().any(|l| {
            let l = l.trim_start();
            !l.starts_with("//") && (l.contains("xfs_repair") || l.contains("repair::script("))
        });
        if !runs_it {
            continue;
        }
        if !body.contains("repair::assert_agreed")
            && !body.contains(common::repair::IGNORED_THE_LOG)
        {
            unchecked.push(name);
        }
    }
    assert!(
        unchecked.is_empty(),
        "these oracles read an xfs_repair report without the check that refuses a blind \
         one, so a report saying it ignored the log counts as a pass: {unchecked:?}"
    );
}

/// And the check has to recognise the real thing, said by the real tool.
///
/// A volume crashed with its log full is what every oracle's image looks
/// like before the kernel replays it, so this is the same report they
/// would get if the replay had not happened — the failure mode the check
/// exists to name.
#[test]
fn a_report_from_an_unreplayed_log_is_refused() {
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let name = format!("repair-verdict/dirty-{}.img", std::process::id());
    let image = share().join(&name);
    std::fs::create_dir_all(image.parent().unwrap()).unwrap();
    let _scratch = Scratch(image.clone());
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(300 * 1024 * 1024))
        .unwrap();

    let Some(out) = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f /share/{name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop /share/{name} "$m" && echo MOUNT_OK
        for i in $(seq 0 199); do : > "$m/f_$i"; done
        xfs_io -x -c 'shutdown -f' "$m" && echo SHUTDOWN_OK
        umount "$m" || umount -l "$m"
        rmdir "$m"
        {repair}
        echo DONE
        "#,
        repair = repair::script(&format!("/share/{name}")),
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        out.contains("MKFS_OK") && out.contains("MOUNT_OK") && out.contains("SHUTDOWN_OK"),
        "building the crashed volume failed:\n{out}"
    );

    let refused = std::panic::catch_unwind(|| repair::assert_agreed(&out, "a crashed volume"));
    assert!(
        refused.is_err(),
        "xfs_repair was asked about an unreplayed log and the check accepted its \
         answer:\n{out}"
    );
}

struct Scratch(std::path::PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        if let Some(dir) = self.0.parent() {
            let _ = std::fs::remove_dir(dir);
        }
    }
}
