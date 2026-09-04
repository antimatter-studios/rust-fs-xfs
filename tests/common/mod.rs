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
/// `None` means no kernel was reachable — a reason to skip. It
/// deliberately does **not** cover a script that ran and reported a
/// problem: the scripts never exit non-zero, so a kernel refusing the
/// filesystem arrives as output to assert on rather than as a missing
/// host. Conflating the two is how a real failure got reported as a
/// skip the first time these suites ran. (Carried here from the four
/// copies of this function that it replaces, because it is the same
/// mistake this whole module exists to stop.)
///
/// A script that runs but does not print `DONE` is a bug in the script
/// rather than a missing host, so that is an assertion, not a skip.
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
        Transport::Vm => {
            // One caller at a time. Vagrant holds a lock per machine and
            // FAILS rather than waits when it is taken, so two test
            // binaries reaching for the VM at once turn into
            // "Translation missing: en.vagrant.errors.machine_action_locked"
            // and a skip -- which reads as a pass. Cargo runs each test
            // binary's tests in parallel, so this is ordinary, and it is
            // intermittent, which is worse: the suite loses a little
            // coverage at random and says so only in a line nobody reads.
            let _guard = VmLock::acquire();
            Command::new(repo().join("scripts/vm.sh"))
                .arg("run")
                .arg(script)
                .output()
                .ok()?
        }
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

/// A lock held across one `vm.sh` invocation.
///
/// A file rather than a mutex: the contention is between separate test
/// BINARIES, which are separate processes, so nothing in this one's
/// memory can serialise them.
///
/// Advisory and deliberately simple -- create the file exclusively, or
/// wait and try again. A holder that dies without cleaning up would
/// wedge every later caller, so the file is treated as stale after
/// `STALE_AFTER` and taken; the longest legitimate hold is a VM boot
/// plus a script, and the timeout is well past that.
struct VmLock(PathBuf);

impl VmLock {
    const STALE_AFTER: std::time::Duration = std::time::Duration::from_secs(600);

    fn acquire() -> Self {
        let path = share().join(".kernel-run.lock");
        let _ = std::fs::create_dir_all(share());
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Self(path),
                Err(_) => {
                    let stale = std::fs::metadata(&path)
                        .and_then(|m| m.modified())
                        .map(|t| t.elapsed().unwrap_or_default() > Self::STALE_AFTER)
                        .unwrap_or(true);
                    if stale {
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
}

impl Drop for VmLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
