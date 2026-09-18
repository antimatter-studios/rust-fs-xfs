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

/// How long one call into the VM may take before it is given up on.
const VM_CALL_TIMEOUT_SECONDS: u32 = 600;

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
/// Stop dead if the process that started this test has gone.
///
/// # The mess this prevents
///
/// Killing `cargo test` does not kill the test BINARY it spawned. The
/// binary keeps running, keeps calling `vm.sh`, and `vm.sh` boots the VM
/// on demand — so every `vagrant halt` was followed by a fresh QEMU a
/// few seconds later, and the machine sat at a load of 8 with nothing
/// visibly running. `pkill -f "cargo test"` does not match
/// `target/release/deps/feature_matrix_oracle-<hash>`, so the obvious
/// way to stop a run does not stop it.
///
/// An orphaned test has nobody to report to and no reason to keep
/// booting a virtual machine. It exits.
///
/// The parent is recorded on first use rather than compared against
/// pid 1: a process reparented to `launchd` is the same situation, and
/// on macOS it does not always land on 1.
fn abort_if_orphaned() {
    static PARENT: OnceLock<Option<u32>> = OnceLock::new();

    let parent = *PARENT.get_or_init(|| {
        Command::new("ps")
            .args(["-o", "ppid=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
    });
    let Some(parent) = parent else { return };

    let alive = Command::new("kill")
        .args(["-0", &parent.to_string()])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(true);
    if !alive {
        eprintln!(
            "the process that started this test ({parent}) is gone, so this one is \
             orphaned and would go on booting the oracle VM with nobody watching. Stopping."
        );
        std::process::exit(1);
    }
}

pub fn kernel_run(script: &str) -> Option<String> {
    abort_if_orphaned();

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
                // SUDO RESETS PATH to its secure_path, so a tool installed for
                // this user (xfs_repair under ~/.local/bin, say) is not found
                // as root, and every script that runs it reports "command not
                // found" as though the filesystem were broken. Carry PATH and
                // HOME through: wrappers that locate their binaries under
                // $HOME need the second.
                let mut c = Command::new("sudo");
                c.args(["-n", "env"])
                    .arg(format!(
                        "PATH={}",
                        std::env::var("PATH").unwrap_or_default()
                    ))
                    .arg(format!(
                        "HOME={}",
                        std::env::var("HOME").unwrap_or_default()
                    ))
                    .args(["bash", "-c"]);
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
            // BOUNDED. A call that never returns is how a test run
            // becomes a process nobody knows about: no output, no
            // failure, and a virtual machine held open behind it. Ten
            // minutes is far beyond the slowest legitimate call here — a
            // cold boot plus a replay is about two.
            Command::new("timeout")
                .arg(VM_CALL_TIMEOUT_SECONDS.to_string())
                .arg(repo().join("scripts/vm.sh"))
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
    // AN UNMOUNT THAT DID NOT HAPPEN IS NOT A RESULT (#206).
    //
    // The superblock's summary counters are lazy: they live in memory
    // while a filesystem is mounted, the kernel writes them at unmount,
    // and this driver never writes them. So a busy unmount or a silent
    // read-only fallback leaves them as they were, and the `xfs_repair
    // -n` that follows walks the trees, counts the real free blocks, and
    // disagrees with a superblock the kernel had not finished with.
    //
    // What that prints is `sb_fdblocks N, counted N-1` and nothing else,
    // which reads as a driver fault and was chased as one twice, in #199
    // and #124. Every script says `umount ... || echo UMOUNT_FAILED`, and
    // this is what reads it — here rather than in each suite, because it
    // means the same thing in all of them.
    assert!(
        !stdout.contains("UMOUNT_FAILED"),
        "the guest could not unmount the volume, so the kernel never wrote the summary \
         counters and whatever graded it next was grading a filesystem still in \
         flight:\n{stdout}"
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

// ---------------------------------------------------------------------
// Reading an xfs_repair report (#124)
// ---------------------------------------------------------------------

/// Asking `xfs_repair` about a volume, and reading the answer.
///
/// `allow(dead_code)`: this module is compiled into every test binary
/// that says `mod common;`, including the ones that never run the tool,
/// and unused here means unused *in that binary* rather than unused.
#[allow(dead_code)]
pub mod repair {
    /// What `xfs_repair` said it was doing when it was asked about an
    /// image whose log had not been replayed.
    ///
    /// The tool's own words, and the reason a report carrying them is
    /// not a verdict: *"The filesystem has valuable metadata changes in
    /// a log which is being ignored because the -n option was used.
    /// Expect spurious inconsistencies which may be resolved by first
    /// mounting the filesystem to replay the log."*
    pub const IGNORED_THE_LOG: &str = "valuable metadata changes in a log";

    /// The shell an oracle ends with: ask `xfs_repair` about `img`, and
    /// print everything it said.
    ///
    /// Everything, rather than only on failure, because the report has
    /// to be read whichever way it came out — a zero return code beside
    /// [`IGNORED_THE_LOG`] is a suite reporting green while checking
    /// nothing, and the return code alone cannot tell anyone that.
    pub fn script(img: &str) -> String {
        format!(
            r#"
        echo "REPAIR_BEGIN"
        xfs_repair -n {img} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        "#
        )
    }

    /// The report inside `out`, between the markers [`script`] prints,
    /// or the whole of `out` when an oracle prints its report some other
    /// way.
    pub fn report(out: &str) -> String {
        if !out.contains("REPAIR_BEGIN") {
            return out.to_string();
        }
        out.lines()
            .skip_while(|l| !l.trim().starts_with("REPAIR_BEGIN"))
            .take_while(|l| !l.trim().starts_with("REPAIR_END"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Whether the report is one where the tool declined to look.
    pub fn was_blind(out: &str) -> bool {
        report(out).contains(IGNORED_THE_LOG)
    }

    /// Run `xfs_repair -n` here rather than in a guest, and read its
    /// answer the same way.
    ///
    /// `program` because one oracle runs a build of xfsprogs newer than
    /// the host's and has to name it.
    pub fn assert_agreed_running(program: &str, img: &str, what: &str) {
        let out = std::process::Command::new(program)
            .args(["-n", img])
            .output()
            .unwrap_or_else(|e| panic!("{what}: {program} would not run: {e}"));
        let said = format!(
            "REPAIR_BEGIN\n{}{}\nREPAIR_RC={}\nREPAIR_END",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
            out.status.code().unwrap_or(-1)
        );
        assert_agreed(&said, what);
    }

    /// Read the report in `out` as a verdict on the volume, and fail
    /// unless it is one (#124).
    ///
    /// Three outcomes, of which only the first is a pass:
    ///
    /// - the tool walked the volume and found nothing wrong;
    /// - it found something, which is the failure every oracle here is
    ///   for;
    /// - **it says it ignored the log**, which is neither. `-n` does not
    ///   replay, so on an image whose log still holds records the tool
    ///   is grading structures the log was about to replace. Read as a
    ///   failure that is a driver blamed for a tool declining to look —
    ///   `sb_fdblocks 84960, counted 84959` in #124 — and read as a pass
    ///   it is a suite checking nothing. The image has to be replayed
    ///   first, which for these oracles means mounting and unmounting it
    ///   before it is graded.
    pub fn assert_agreed(out: &str, what: &str) {
        let report = report(out);
        assert!(
            report.contains("REPAIR_RC="),
            "{what}: no xfs_repair report at all, so nothing graded the volume:\n{out}"
        );
        assert!(
            !report.contains(IGNORED_THE_LOG),
            "{what}: xfs_repair was asked about an image whose log it then ignored, so \
             neither its silence nor its complaints say anything about this driver. The \
             image has to be mounted and unmounted — replayed — before it is graded:\n\
             {report}"
        );
        assert!(
            report.contains("REPAIR_RC=0"),
            "{what}: xfs_repair found something wrong with the volume:\n{report}"
        );
    }
}

// ---------------------------------------------------------------------
// Scratch volumes (#223)
// ---------------------------------------------------------------------

/// An image a suite writes to, kept away from the fixtures.
///
/// `allow(dead_code)`: compiled into every test binary that says
/// `mod common;`, including the read-only ones that never make a scratch
/// volume.
#[allow(dead_code)]
pub mod scratch {
    use super::share;
    use std::path::{Path, PathBuf};

    /// Where a suite's scratch volumes live: `.vm-share/scratch/<suite>/`.
    ///
    /// NOT BESIDE THE FIXTURES, which is the whole point. Several suites
    /// walk every `*.img` in `.vm-share` and grade the driver against
    /// whatever they find; cargo runs test binaries in parallel; and an
    /// image another suite is halfway through writing is not a fixture.
    /// That is order-dependent failure with no symptom beyond a number
    /// being off by one — see #124, `sb_fdblocks 84960, counted 84959`.
    ///
    /// The scanners use `read_dir`, which does not recurse, so a
    /// subdirectory is invisible to them. One directory per suite so a
    /// failure names its owner, and so two suites cannot collide on a
    /// name either.
    pub fn dir(suite: &str) -> PathBuf {
        let d = share().join("scratch").join(suite);
        std::fs::create_dir_all(&d)
            .unwrap_or_else(|e| panic!("making the scratch directory for {suite}: {e}"));
        d
    }

    /// Where a path under the fixture directory is inside the guest,
    /// which mounts that directory at `/share`.
    ///
    /// For the helpers that are handed a path rather than the volume
    /// that made it.
    pub fn guest_path(path: &Path) -> String {
        let under = path
            .strip_prefix(share())
            .unwrap_or_else(|_| panic!("{} is not under the fixture directory", path.display()));
        format!("/share/{}", under.display())
    }

    /// A scratch volume, removed when this is dropped.
    pub struct Volume {
        path: PathBuf,
        suite: String,
    }

    impl Volume {
        /// A copy of `source` to write to.
        pub fn copy_of(suite: &str, source: &Path, name: &str) -> Volume {
            let path = dir(suite).join(name);
            std::fs::copy(source, &path).unwrap_or_else(|e| {
                panic!("copying {} to {}: {e}", source.display(), path.display())
            });
            Volume {
                path,
                suite: suite.to_string(),
            }
        }

        /// An empty file of `bytes`, for a guest to make a filesystem in.
        pub fn empty(suite: &str, name: &str, bytes: u64) -> Volume {
            let path = dir(suite).join(name);
            std::fs::File::create(&path)
                .and_then(|f| f.set_len(bytes))
                .unwrap_or_else(|e| panic!("making {}: {e}", path.display()));
            Volume {
                path,
                suite: suite.to_string(),
            }
        }

        /// Where it is on this machine.
        pub fn path(&self) -> &Path {
            &self.path
        }

        /// Where it is inside the guest, which mounts the same directory
        /// at `/share`.
        pub fn guest(&self) -> String {
            format!(
                "/share/scratch/{}/{}",
                self.suite,
                self.path
                    .file_name()
                    .expect("a scratch volume has a name")
                    .to_string_lossy()
            )
        }

        /// Keep it, and say where. For a failure worth looking at
        /// afterwards: the drop below would otherwise take the evidence
        /// with it.
        pub fn keep(self) -> PathBuf {
            let path = self.path.clone();
            std::mem::forget(self);
            path
        }
    }

    impl Drop for Volume {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
            // Empty afterwards or not; a suite still running keeps its own.
            let _ = std::fs::remove_dir(self.path.parent().expect("a parent"));
        }
    }
}
