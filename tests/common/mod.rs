//! Running a script against a real Linux kernel, wherever one is.
//!
//! The replay oracles all do the same thing: this driver writes a log
//! record, then a kernel is asked to replay it and `xfs_repair` is asked
//! whether the result is sound. Only a real kernel can settle that — our
//! own reader agreeing with us proves nothing about whether the record
//! was correct.
//!
//! # Why this is not just `vm.sh`
//!
//! Each oracle used to call `scripts/vm.sh run` directly, so a kernel
//! meant *the VM's* kernel. On a developer Mac that is the only option.
//! On a Linux CI runner it is the wrong one: the runner already has a
//! kernel, xfsprogs, and passwordless sudo, and there is no VM to boot.
//!
//! The tests skipped there. Silently — a skip prints a line and the test
//! returns ok — so CI reported green on the write path while never
//! replaying a single record. `truncate_replay_oracle` and
//! `unlink_replay_oracle` had done that on every run since they were
//! written.
//!
//! So the transport is chosen from what the host can do, and the script
//! is the same either way.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The shared fixture directory. Inside the VM it is mounted at
/// `/share`; natively it is this path, and scripts written against
/// `/share` are rewritten to match.
pub fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// How a script gets to a kernel on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Run it here. Linux, with the tools and the privilege.
    Native,
    /// Ship it to the oracle VM. Anything else with a working `vm.sh`.
    Vm,
    /// Neither is possible, and the caller must skip and say so.
    None,
}

/// Decide once. Probing sudo per call would prompt repeatedly and slow
/// every case down.
pub fn transport() -> Transport {
    static CHOICE: OnceLock<Transport> = OnceLock::new();
    *CHOICE.get_or_init(|| {
        if cfg!(target_os = "linux") && have("mount") && have("xfs_repair") && can_elevate() {
            Transport::Native
        } else if repo().join("scripts/vm.sh").exists() {
            Transport::Vm
        } else {
            Transport::None
        }
    })
}

fn have(tool: &str) -> bool {
    // `command -v` rather than running the tool: xfs_repair with no
    // argument exits non-zero, and mount with none prints the table.
    Command::new("sh")
        .arg("-c")
        .arg(format!("command -v {tool}"))
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Root already, or sudo without a password. A sudo that would prompt is
/// not usable from a test: it would block forever on a runner and steal
/// the terminal on a workstation.
fn can_elevate() -> bool {
    if is_root() {
        return true;
    }
    Command::new("sudo")
        .args(["-n", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "0")
        .unwrap_or(false)
}

/// Run `script` against a kernel and return its stdout.
///
/// `None` means no kernel was reachable, or the script failed — the
/// caller skips and says which. A script that runs but does not print
/// `DONE` is a bug in the script rather than a missing host, so that is
/// an assertion, not a skip.
pub fn kernel_run(script: &str) -> Option<String> {
    let out = match transport() {
        Transport::Native => {
            // The scripts are written for the VM, where the fixtures are
            // at /share. Point them at the real directory instead.
            let localised = script.replace("/share/", &format!("{}/", share().display()));
            let mut cmd = if is_root() {
                let mut c = Command::new("bash");
                c.arg("-c");
                c
            } else {
                let mut c = Command::new("sudo");
                c.args(["-n", "bash", "-c"]);
                c
            };
            cmd.arg(localised).output().ok()?
        }
        Transport::Vm => Command::new(repo().join("scripts/vm.sh"))
            .arg("run")
            .arg(script)
            .output()
            .ok()?,
        Transport::None => return None,
    };

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        eprintln!(
            "{:?} run failed: {}",
            transport(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    assert!(
        stdout.contains("DONE"),
        "the script did not run to completion under {:?}:\n{stdout}",
        transport()
    );
    Some(stdout)
}
