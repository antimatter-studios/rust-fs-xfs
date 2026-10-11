//! What the command-line tests share: the multi-call binary, reached
//! under each of its names, and a scratch directory to put things in.
//!
//! The binary is built only with the `cli` feature. `scripts/test.sh`,
//! `scripts/ci-test.sh` and `chore test:unit` turn it on; a bare `cargo
//! test` does not, and then these tests FAIL naming the fix rather than
//! skipping.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::OnceLock;

/// The repository-named entry point, as cargo built it.
///
/// `option_env!`, not `env!`: without the feature `env!` would fail the
/// compile of every test target in the run, where this fails only the
/// tests that need the binary, each with the fix in its message.
const BIN: Option<&str> = option_env!("CARGO_BIN_EXE_rust-fs-xfs");

const NO_BIN: &str = "the rust-fs-xfs binary is built only with `--features cli`. Run the \
    tests through scripts/test.sh or a chore tier, which pass it, or add `--features cli` \
    to cargo test.";

pub fn bin() -> &'static str {
    BIN.expect(NO_BIN)
}

/// The binary under its own (cargo's) name: the repository entry point.
pub fn entry() -> Command {
    Command::new(BIN.expect(NO_BIN))
}

/// The program as a user runs it under `name`: argv[0] is what an
/// installed symlink hands it, and what it dispatches on.
pub fn tool(name: &str) -> Command {
    use std::os::unix::process::CommandExt;
    let mut cmd = Command::new(BIN.expect(NO_BIN));
    cmd.arg0(name);
    cmd
}

/// A fresh directory for one test's scratch files, under the temporary
/// directory: the one scripts/with-test-temp.sh chose inside the checkout
/// under `chore`, the system's otherwise.
///
/// REMOVED WHEN THE TEST IS DONE (#447), pass or fail: the guard deletes
/// it on drop, unwinding included. Under `chore` the tier's directory went
/// anyway; a suite run straight through `cargo test` left every case's
/// volume in the system temp directory, 3.7 GB of them in one session.
/// It has no `Deref` on purpose: `scratch_dir(..).join(name)` would drop
/// the guard at the end of that expression and delete the directory
/// before it was used, so a caller keeps the guard and asks for
/// [`Scratch::path`].
pub fn scratch_dir(tag: &str) -> Scratch {
    Scratch(fresh_dir(tag))
}

/// A test's scratch directory, removed with this.
pub struct Scratch(PathBuf);

impl Scratch {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn fresh_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cli-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create {}: {e}", dir.display()));
    dir
}

/// A directory holding the binary under every name it answers to, as an
/// install links it: `rust-fs-xfs` and each dotted name, symlinks to
/// cargo's build.
pub fn names_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        // Kept for the whole run in a static that is never dropped: a few
        // symlinks to the build, not volumes.
        let dir = fresh_dir("names");
        let mut names = dotted_names();
        names.push("rust-fs-xfs".to_string());
        for name in names {
            std::os::unix::fs::symlink(bin(), dir.join(&name))
                .unwrap_or_else(|e| panic!("link {name}: {e}"));
        }
        dir
    })
}

/// The dotted names, as the binary itself lists them for packaging.
pub fn dotted_names() -> Vec<String> {
    let out = entry()
        .args(["generate", "names"])
        .output()
        .expect("run rust-fs-xfs generate names");
    assert!(out.status.success(), "generate names failed: {out:?}");
    String::from_utf8(out.stdout)
        .expect("names are UTF-8")
        .lines()
        .map(str::to_string)
        .collect()
}

pub fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Run and require success, returning the output.
#[track_caller]
pub fn ok(cmd: &mut Command) -> Output {
    let out = cmd.output().expect("spawn");
    assert!(
        out.status.success(),
        "{cmd:?} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        stdout(&out),
        stderr(&out)
    );
    out
}

/// The value of `"key": ...` in a JSON report: enough of a reader for the
/// flat reports these tests check, without a JSON dependency. Strings
/// come back without their quotes; anything else as written.
#[track_caller]
pub fn json_field(json: &str, key: &str) -> String {
    let needle = format!("\"{key}\": ");
    let start = json
        .find(&needle)
        .unwrap_or_else(|| panic!("no {key:?} in:\n{json}"))
        + needle.len();
    let rest = &json[start..];
    if let Some(stripped) = rest.strip_prefix('"') {
        let end = stripped.find('"').expect("closing quote");
        stripped[..end].to_string()
    } else {
        rest.split([',', '\n', '}'])
            .next()
            .unwrap()
            .trim()
            .to_string()
    }
}

/// Bytes nobody would type: a fixed LCG, so a failure reproduces.
pub fn pattern(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8
        })
        .collect()
}

/// `fs.xfs <image> write <path>` with `bytes` on stdin; the output.
pub fn fs_write(image: &str, path: &str, bytes: &[u8]) -> Output {
    use std::io::Write;
    let mut child = tool("fs.xfs")
        .args([image, "write", path])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn fs.xfs write");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(bytes)
        .expect("feed stdin");
    child.wait_with_output().expect("wait for fs.xfs write")
}

/// The SHA-256 of `bytes`, in the lowercase hex `sha256sum` prints.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
