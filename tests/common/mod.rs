//! Running a script, and every oracle tool, against a real Linux kernel
//! — IN THE fs-linux-test-harness GUEST, and nowhere else.
//!
//! # Why the guest is the only place
//!
//! The oracles all do the same thing: this driver writes something, then
//! `mkfs.xfs`, `xfs_db`, `xfs_repair`, `xfs_logprint` or the in-kernel
//! XFS driver is asked whether the result is sound. Only an independent
//! implementation can settle that — our own reader agreeing with us
//! proves nothing.
//!
//! An oracle whose answer depends on which machine asked is not an
//! oracle. xfsprogs on a workstation is whatever that machine has:
//! nothing at all on a Mac, 6.1 on Debian 12, 6.6 on Ubuntu 24.04, and
//! 6.13 if someone built one under `~/.local`. The kernel is worse: this
//! host runs 6.12 and the CI runner runs whatever Azure booted that
//! week, and #211 and #212 are both "the fixture came out different on
//! the kernel that built it".
//!
//! So this module chooses nothing. Every tool call and every mount goes
//! to ONE Debian guest, provisioned by `scripts/vm-setup.sh` and booted
//! by the harness, on a developer's machine and on a CI runner alike.
//!
//! This replaces the `Transport::{Native, Vm, None}` fork that used to
//! live here, which ran the scripts under `sudo -n` when the host looked
//! Linux enough and in the VM otherwise — two kernels, two xfsprogs, and
//! a third case (`None`) whose only outcome was a test that skipped and
//! reported ok.
//!
//! # Nothing skips
//!
//! [`kernel_run`] returns the script's output, not an `Option`. There is
//! no "no kernel reachable" any more, because there is exactly one
//! kernel and failing to reach it is a failure: a harness that is not
//! checked out, a VM that will not boot, a tool the guest does not have
//! — each panics naming the task that fixes it.
//!
//! THAT IS ALSO THE FIX FOR #206's second half. `kernel_run` used to
//! return `None` both when no host could be found AND when the script's
//! process failed, so `write_oracle` printed "oracle VM unavailable —
//! skipping verification" for a mount the kernel had refused. The two
//! are now different things: a script that runs is reported by its own
//! output and its own exit status, and only the harness failing to reach
//! the guest is a missing host.
//!
//! # One path means one thing on both sides
//!
//! The harness mounts this repository in the guest at `/repo`, and
//! [`session`] symlinks the host's own absolute path to it, so
//! `<repo>/.vm-share/xfs-default.img` is that same path in the guest and
//! arguments cross unchanged. Scripts written against `/share` — which
//! is what every fixture builder and every oracle script in this
//! repository says — are localised to that path by [`kernel_run`], which
//! is the same rewrite the old native transport did, now applied on
//! every path instead of one of them.
//!
//! # Why it is not slow
//!
//! The VM is booted once for a tier (`chore test:oracle` brings it up
//! and the reaper stops it) and every call rides one multiplexed SSH
//! connection: about 30 ms of overhead per call against 700 ms for a
//! fresh handshake. Nothing is copied.

// EVERY TEST BINARY COMPILES THE WHOLE MODULE and uses part of it, so
// an item only the oracle tiers call is dead code in the unit tier's
// binaries. The alternative — a feature per helper, or one module per
// caller — would fragment the single place the guest is spoken to, which
// is the property this module exists to have.
#![allow(dead_code)]

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Where `scripts/vm-setup.sh` installs the xfsprogs build that knows
/// parent pointers (6.10 and newer; Debian 12 ships 6.1).
///
/// THIS PATH AND scripts/vm-setup.sh MUST AGREE. That script greps this
/// file for it, so the pair cannot drift silently.
pub const PARENT_XFSPROGS_BIN: &str = "/usr/local/xfsprogs-parent/sbin";

/// The shared fixture directory: `<repo>/.vm-share` on the host, which
/// the guest sees both as `/share` and — through the repository mount —
/// at this same absolute path.
pub fn share() -> PathBuf {
    repo().join(".vm-share")
}

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// The harness's driver script, or a panic naming `chore siblings`.
fn vm_script() -> &'static Path {
    static VM: OnceLock<PathBuf> = OnceLock::new();
    VM.get_or_init(|| {
        let path = repo().join("../fs-linux-test-harness/scripts/vm.sh");
        assert!(
            path.is_file(),
            "the fs-linux-test-harness sibling is not checked out at {}. \
             `chore siblings` clones it at the ref chores.yml pins. The oracle \
             tools and the kernel run in its VM and nowhere else, so there is \
             nothing to fall back to and nothing to skip.",
            path.display()
        );
        path
    })
}

/// True when this process is itself running inside the harness guest
/// (`chore test:vm`, which is how a Mac runs this suite at all).
fn in_guest() -> bool {
    std::env::var_os("FLTH_GUEST").is_some_and(|value| value == "1")
}

/// Run a harness command from the repository root, where the harness
/// finds `fs-linux-test-harness.toml`.
fn vm(command: &str, argument: &str) -> io::Result<Output> {
    Command::new(vm_script())
        .arg(command)
        .arg(argument)
        .current_dir(repo())
        .stdin(Stdio::null())
        .output()
}

/// Boot the VM once per test process, and make the host's own path for
/// this repository mean the repository inside the guest too.
///
/// `vm.sh up` is idempotent and costs milliseconds when the VM is
/// already running, which is the normal case: the tier task brings it up
/// for the whole run. A test process that finds it down boots it rather
/// than failing, so a suite run by hand with a bare `cargo test` still
/// works, and the chore reaper stops what it left behind.
fn session() {
    static SESSION: OnceLock<()> = OnceLock::new();
    SESSION.get_or_init(|| {
        if in_guest() {
            return;
        }
        let out = vm("up", "")
            .unwrap_or_else(|error| panic!("cannot run {}: {error}", vm_script().display()));
        assert!(
            out.status.success(),
            "the fs-linux-test-harness VM would not start, so no oracle tool and no \
             kernel can be reached.\n`chore vm:host:check` says what this host is \
             missing; `chore vm:destroy` clears a broken machine.\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        mirror_repo_path();
    });
}

/// MAKE ONE PATH MEAN ONE THING ON BOTH SIDES.
///
/// The harness mounts this repository in the guest at `/repo`. A test
/// hands `xfs_repair` the path it used on the host — `<repo>/tmp/x.img`
/// — so the guest is given that same absolute path as a symlink to the
/// mount. Every argument then crosses unchanged: no rewriting of
/// arguments, nothing copied in and out.
///
/// Idempotent, and it refuses to replace a real directory: in a guest
/// that somehow has one at that path, silently shadowing it would be
/// worse than stopping.
fn mirror_repo_path() {
    let repo = repo().to_string_lossy().into_owned();
    let script = format!(
        "set -eu\n\
         repo={0}\n\
         if [ -e \"$repo\" ] && [ ! -L \"$repo\" ]; then\n\
             echo \"$repo exists in the guest and is not the repository mount\" >&2\n\
             exit 1\n\
         fi\n\
         mkdir -p \"$(dirname \"$repo\")\"\n\
         ln -sfn /repo \"$repo\"\n\
         [ -f \"$repo/Cargo.toml\" ]",
        guest_quote(&repo)
    );
    let out = vm("exec", &script)
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", vm_script().display()));
    assert!(
        out.status.success(),
        "the guest cannot see this repository at {repo}, so no oracle tool can read \
         the images a test writes. The harness mounts the consumer repository at /repo \
         on every boot (`chore vm:destroy` then `chore vm:up` re-provisions it).\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Run a script in the guest, wherever this process is.
///
/// From the host that is `vm.sh exec` over the harness's one shared
/// connection. Inside the guest it is the shell itself, as root.
fn guest_shell(script: &str) -> io::Result<Output> {
    if in_guest() {
        return Command::new("bash")
            .arg("-c")
            .arg(script)
            .current_dir(repo())
            .stdin(Stdio::null())
            .output();
    }
    vm("exec", script)
}

/// One argument, as the guest's shell will read it.
pub fn guest_quote(argument: &str) -> String {
    format!("'{}'", argument.replace('\'', r"'\''"))
}

/// The three files one guest call leaves behind, named so that two calls
/// — from two threads or two test binaries — never share one.
struct Run {
    dir: PathBuf,
    stdout: PathBuf,
    stderr: PathBuf,
    status: PathBuf,
}

impl Run {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let dir = share().join("run");
        let name = format!(
            "{}.{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        Self {
            stdout: dir.join(format!("{name}.out")),
            stderr: dir.join(format!("{name}.err")),
            status: dir.join(format!("{name}.status")),
            dir,
        }
    }

    /// The script's own exit status, or `None` when the guest never ran
    /// it — which is the harness failing, not the script.
    fn code(&self) -> Option<i32> {
        let text = std::fs::read_to_string(&self.status).ok()?;
        let code = text.trim().parse().ok()?;
        let _ = std::fs::remove_file(&self.status);
        Some(code)
    }

    fn streams(&self) -> (String, String) {
        let read = |path: &PathBuf| {
            let bytes = std::fs::read(path).unwrap_or_default();
            let _ = std::fs::remove_file(path);
            String::from_utf8_lossy(&bytes).into_owned()
        };
        (read(&self.stdout), read(&self.stderr))
    }
}

/// What one guest call did: the script's own exit status and its two
/// streams, kept apart from whether the harness reached the guest.
///
/// THAT SEPARATION IS THE POINT (#206). The harness answers 1 for "no
/// VM"; `xfs_repair` answers 1 for "this filesystem has errors". If both
/// arrive as the exit status of one process there is no way to tell a
/// refused mount from an absent machine, and this suite spent a release
/// reporting the first as the second.
pub struct GuestOutput {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl GuestOutput {
    pub fn ok(&self) -> bool {
        self.status == 0
    }

    /// What the tool said, in the shape [`repair::assert_agreed`] reads.
    ///
    /// The markers exist because a guest script's output is a whole
    /// session and the report is a part of it. A tool run directly has
    /// no session around it, so the markers are put back here and one
    /// reader grades both (#124).
    pub fn repair_report(&self) -> String {
        format!(
            "REPAIR_BEGIN\n{}{}\nREPAIR_RC={}\nREPAIR_END",
            self.stdout, self.stderr, self.status
        )
    }
}

/// Run `script` in the guest under `bash -euo pipefail`, and return what
/// it did.
///
/// Panics — never skips — when the harness cannot reach the guest at
/// all. A script that ran and failed comes back in [`GuestOutput`].
#[track_caller]
pub fn guest_script(script: &str) -> GuestOutput {
    session();
    let run = Run::new();
    let wrapped = format!(
        "mkdir -p {dir} && cd {repo} && \
         {{ bash -euo pipefail -c {script} > {stdout} 2> {stderr}; }}; \
         printf %s $? > {status}",
        dir = guest_quote(&run.dir.to_string_lossy()),
        repo = guest_quote(&repo().to_string_lossy()),
        script = guest_quote(script),
        stdout = guest_quote(&run.stdout.to_string_lossy()),
        stderr = guest_quote(&run.stderr.to_string_lossy()),
        status = guest_quote(&run.status.to_string_lossy()),
    );
    let out = guest_shell(&wrapped)
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", vm_script().display()));
    let Some(status) = run.code() else {
        panic!(
            "the script could not be run in the fs-linux-test-harness VM (the harness \
             exited {:?}). This is the harness, not the script: `chore vm:status` shows \
             the VM and `chore vm:up` boots it. Tests never skip on a missing VM.\n{}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };
    let (stdout, stderr) = run.streams();
    GuestOutput {
        status,
        stdout,
        stderr,
    }
}

/// Run `script` against the kernel in the guest and return its stdout.
///
/// The scripts in this repository are written for the shared directory
/// as the guest used to see it, `/share`; that is rewritten to the path
/// both sides agree on, so one script text works whether this process is
/// on the host or inside the guest.
///
/// A script that runs to completion prints `DONE`; one that does not is
/// a bug in the script rather than a missing host, so that is an
/// assertion. The scripts themselves never exit non-zero on a filesystem
/// problem — a kernel refusing a mount arrives as output to assert on —
/// so a non-zero status here is the script breaking, and it fails loudly
/// with both streams rather than being reported as an absent VM (#206).
#[track_caller]
pub fn kernel_run(script: &str) -> String {
    let localised = script.replace("/share/", &format!("{}/", share().display()));
    let out = guest_script(&localised);
    assert!(
        out.ok(),
        "the guest script exited {} — that is the script failing, not a missing \
         kernel:\n--- stdout ---\n{}--- stderr ---\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
    assert!(
        out.stdout.contains("DONE"),
        "the script did not run to completion in the guest:\n--- stdout ---\n{}\
         --- stderr ---\n{}",
        out.stdout,
        out.stderr
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
        !out.stdout.contains("UMOUNT_FAILED"),
        "the guest could not unmount the volume, so the kernel never wrote the summary \
         counters and whatever graded it next was grading a filesystem still in \
         flight:\n{}",
        out.stdout
    );
    out.stdout
}

/// A tool invocation, built and then run in the guest.
///
/// ```ignore
/// let out = oracle("xfs_repair").args(["-n", &image]).output();
/// assert_eq!(out.status, 0, "{}", out.stderr);
/// ```
///
/// THE ONLY WAY A TEST REACHES AN ORACLE TOOL. `tests/test_contract.rs`
/// fails the suite if a test spawns one itself, which would run it on the
/// host — a different version, a different platform, and free to be
/// absent so that the test could skip.
#[must_use]
pub struct Oracle {
    tool: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Option<String>,
}

/// Start building a call to `tool` (`mkfs.xfs`, `xfs_db`, `xfs_repair`,
/// `xfs_logprint`, `xfs_io`), which runs in the harness VM.
pub fn oracle(tool: &str) -> Oracle {
    Oracle {
        tool: tool.to_string(),
        args: Vec::new(),
        env: Vec::new(),
        stdin: None,
    }
}

/// The same tool, from the pinned xfsprogs the guest builds for parent
/// pointers and exchange-range (6.10 and newer). Debian's is 6.1, and a
/// test that needs the newer one says so here rather than probing.
pub fn parent_oracle(tool: &str) -> Oracle {
    oracle(&format!("{PARENT_XFSPROGS_BIN}/{tool}"))
}

impl Oracle {
    #[track_caller]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.args.push(text(argument.as_ref()));
        self
    }

    #[track_caller]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for argument in arguments {
            self.args.push(text(argument.as_ref()));
        }
        self
    }

    /// An environment variable for the tool, in the guest.
    pub fn env(mut self, name: &str, value: &str) -> Self {
        self.env.push((name.to_string(), value.to_string()));
        self
    }

    /// Text on the tool's standard input (an `xfs_db` command list).
    pub fn stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }

    /// Run it, and return what the tool did.
    #[track_caller]
    pub fn output(self) -> GuestOutput {
        for argument in &self.args {
            self.check_path(argument);
        }
        let mut line = String::new();
        for (name, value) in &self.env {
            line.push_str(&format!("{name}={} ", guest_quote(value)));
        }
        line.push_str(&guest_quote(&self.tool));
        for argument in &self.args {
            line.push(' ');
            line.push_str(&guest_quote(argument));
        }
        let script = match &self.stdin {
            Some(input) => format!("printf %s {} | {line}", guest_quote(input)),
            None => line,
        };
        let out = guest_script(&script);
        assert!(
            out.status != 127,
            "the oracle tool `{}` is not installed in the harness VM. It is \
             provisioned by scripts/vm-setup.sh; `chore vm:provision` applies that \
             script again. Tests never skip on a missing tool.\n{}",
            self.tool,
            out.stderr
        );
        // The evidence a green run carries: every tool call and its
        // verdict, kept in the tier's log by --nocapture.
        println!(
            "[oracle vm] {} {} -> {}",
            self.tool,
            self.args.join(" "),
            out.status
        );
        out
    }

    /// Everything a tool touches is inside this repository, because that
    /// is the tree the guest has. Caught here, where the rule can be
    /// explained, rather than in the guest as a missing file.
    #[track_caller]
    fn check_path(&self, argument: &str) {
        if !argument.starts_with('/') || !Path::new(argument).exists() {
            return;
        }
        let repo = repo();
        assert!(
            Path::new(argument).starts_with(&repo),
            "`{}` was given {argument}, which is outside {}. The oracle tools run in \
             the harness VM, which sees this repository and nothing else of the host, \
             so a path outside it does not exist there. Put scratch files under the \
             directory scripts/with-test-temp.sh selects, which is inside the \
             repository for exactly this reason.",
            self.tool,
            repo.display()
        );
    }
}

/// An argument as text, which every path and flag this suite passes is.
#[track_caller]
fn text(argument: &OsStr) -> String {
    argument
        .to_str()
        .unwrap_or_else(|| {
            panic!("an oracle argument that is not UTF-8 cannot be passed to the guest")
        })
        .to_string()
}

/// `xfs_repair -n` on `image` must find nothing, or the test fails with
/// the report. It runs in the harness VM, like every oracle tool.
#[track_caller]
pub fn assert_xfs_repair_clean(image: &str, tag: &str) {
    let out = oracle("xfs_repair").args(["-n", image]).output();
    // GRADED BY repair::assert_agreed, not by the exit status alone: a
    // zero return code beside "valuable metadata changes in a log" is
    // the tool declining to look at an image whose log it did not
    // replay, and reading that as a pass is a suite checking nothing
    // (#124).
    repair::assert_agreed(&out.repair_report(), tag);
}

/// The path of a fixture under `.vm-share/`, or a panic that says how to
/// build it.
///
/// THE ONLY WAY A TEST REACHES A FIXTURE. The images are gitignored and
/// generated: the kernel populates them, inside the harness VM. A test
/// that found its image absent used to print "skipping" and return, and
/// a skipped test reads exactly like a passing one — so a checkout
/// without fixtures ran most of this suite against nothing and reported
/// green. `tests/truncate_oracle.rs` did that in CI for its whole
/// existence, which is how truncate.rs came to sit at 5% line coverage
/// with a passing oracle.
///
/// `chore fixtures` builds every set, `chore test` checks they are there
/// before it runs a test that needs one, and `scripts/ci-test.sh` fails
/// a run whose output says it skipped. This is the half that makes the
/// test itself honest.
#[track_caller]
pub fn fixture(name: &str) -> PathBuf {
    let path = share().join(name);
    assert!(
        path.is_file(),
        ".vm-share/{name} is missing: the fixtures are gitignored and generated. \
         Build them with `chore fixtures` (it boots the fs-linux-test-harness VM; \
         `chore siblings` checks the harness out), then run the tests again. \
         Tests never skip on a missing fixture.",
    );
    path
}

/// Every fixture in `.vm-share` whose name starts with `prefix` and ends
/// with `suffix`, in sorted order, or a panic naming the task that
/// builds them when there are none.
///
/// The same rule as [`fixture`] for the suites that walk a set rather
/// than naming one image: a set that produced nothing is a fixture
/// build that did not happen, not a test with nothing to do.
#[track_caller]
pub fn fixtures_matching(prefix: &str, suffix: &str) -> Vec<PathBuf> {
    let dir = share();
    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with(prefix) && n.ends_with(suffix))
                })
                .collect()
        })
        .unwrap_or_default();
    found.sort();
    assert!(
        !found.is_empty(),
        "no {prefix}*{suffix} fixture in {}: the fixtures are gitignored and \
         generated. Build them with `chore fixtures`, then run the tests again. \
         Tests never skip on a missing fixture.",
        dir.display()
    );
    found
}
// ---------------------------------------------------------------------
// Reading an xfs_repair report (#124)
// ---------------------------------------------------------------------

/// Asking `xfs_repair` about a volume, and reading the answer.
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
