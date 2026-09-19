//! The test contract, checked: THE ORACLE TOOLS RUN IN THE HARNESS VM
//! AND NOWHERE ELSE, the kernel is only ever asked in the guest, and no
//! test announces a skip.
//!
//! Why the host is forbidden rather than merely second choice: xfsprogs
//! on a workstation is whatever that machine has — nothing at all on a
//! Mac, 6.1 on Debian 12, 6.6 on Ubuntu 24.04, 6.13 if somebody built
//! one under `~/.local` — and the kernel is worse, because the one that
//! builds a fixture and the one that grades it were different machines.
//! One version, in one guest, answers the same way for everyone. So a
//! test that spawns `xfs_repair` itself is refused here even when it
//! would work on the machine that wrote it.
//!
//! This is also what makes `scripts/test-targets.sh` sound. The tiers —
//! `chore test:unit`, `test:images`, `test:oracle`, `test:kernel` — are
//! chosen from what each test file CALLS: `common::oracle` /
//! `parent_oracle` / `assert_xfs_repair_clean` for a tool,
//! `common::kernel_run` / `guest_script` for the kernel, `common::share`
//! or a `.vm-share` path for a fixture. A test that spawned a tool by
//! name would be classified as a unit test, run on the `unit` CI job
//! with no VM, and — worse — be free to return early when the tool is
//! absent, which is the silent pass this repository spent a release
//! chasing. So this file reads every test source and refuses every other
//! shape.
//!
//! It names the patterns it looks for without spelling them out, so that
//! this file itself stays in the unit tier.

use std::path::{Path, PathBuf};

fn manifest_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `dir`, recursively, except the shared test
/// module (which is where the sanctioned helpers live) and this file
/// (whose self-test spells out the shapes it refuses).
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "common") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") && !path.ends_with(file!()) {
            out.push(path);
        }
    }
}

fn all_test_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    rust_sources(&manifest_dir().join("tests"), &mut files);
    rust_sources(&manifest_dir().join("src"), &mut files);
    assert!(
        files.len() > 60,
        "found only {} sources; the scan is looking in the wrong place",
        files.len()
    );
    files
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()));
            (p, text)
        })
        .collect()
}

/// The xfsprogs programs the oracle tests use.
const TOOLS: [&str; 7] = [
    "mkfs.xfs",
    "xfs_db",
    "xfs_repair",
    "xfs_logprint",
    "xfs_io",
    "xfs_admin",
    "xfs_bmap",
];

/// Places in `text` where a process is spawned from a string literal
/// that names an oracle tool, or from a hard-coded sbin path.
fn direct_tool_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let named = TOOLS.contains(&program)
            || program.contains("sbin/")
            || TOOLS.iter().any(|t| program.ends_with(&format!("/{t}")));
        if named {
            hits.push(program.to_string());
        }
    }
    // A probe of a fixed install path is how the old "is xfs_repair
    // here?" skips found their tool.
    for line in text.lines() {
        let probe = ["\"/usr/", "sbin/"].concat();
        let probe_root = ["\"/", "sbin/"].concat();
        if !line.contains(&spawn)
            && (line.contains(&probe) || line.contains(&probe_root))
            && TOOLS.iter().any(|t| line.contains(t))
        {
            hits.push(line.trim().to_string());
        }
    }
    hits
}

/// Places in `text` that spawn a program NAMED BY A VARIABLE.
///
/// The scan above reads the literal a process is spawned with, so a test
/// that puts the tool's name in a variable first would walk past it —
/// and that is not a hypothetical: `tests/parent_exchrange_oracle.rs`
/// did exactly that, with a `tool()` helper that returned a `PathBuf`
/// from `XFSPROGS_PARENT_BIN`. The only programs a test spawns by
/// computed name are this crate's own binaries, which come from
/// `CARGO_BIN_EXE_*`, so the rule is: a non-literal program must be one
/// of those, in the same file.
fn indirect_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let own_binary = ["CARGO_BIN", "_EXE"].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        if rest.starts_with('"') {
            continue;
        }
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            hits.push(rest.lines().next().unwrap_or_default().trim().to_string());
            continue;
        }
        // The binding it came from, wherever it is in the file.
        let bound_to_own_binary = text.lines().any(|line| {
            (line.contains(&format!("let {name} ="))
                || line.contains(&format!("let {name}:"))
                || line.contains(&format!("const {name}:")))
                && line.contains(&own_binary)
        });
        if !bound_to_own_binary {
            hits.push(name);
        }
    }
    hits
}

/// Programs that reach the VM, elevate, or mount a filesystem. A test
/// drives none of them: the harness is spoken to in one place
/// (`tests/common`), so there is one answer to "is the VM up", one place
/// that boots it, and no test that mounts anything on the machine
/// running it.
///
/// `sudo` is in this list on purpose. The kernel oracles used to choose
/// between `sudo -n bash -c ...` on a host that looked Linux enough and
/// the VM otherwise, so the branch gate and a developer's run were
/// judged by two different kernels. There is one kernel now, it is the
/// guest's, and nothing in this repository elevates on the host.
const HARNESS: [&str; 8] = [
    "vagrant", "ssh", "sudo", "mount", "umount", "losetup", "vm.sh", "modprobe",
];

/// Places in `text` that spawn one of those.
fn harness_spawns(text: &str) -> Vec<String> {
    let spawn = ["Command", "::", "new", "("].concat();
    let mut hits = Vec::new();
    for (at, _) in text.match_indices(&spawn) {
        let rest = text[at + spawn.len()..].trim_start();
        let Some(literal) = rest.strip_prefix('"') else {
            continue;
        };
        let Some(end) = literal.find('"') else {
            continue;
        };
        let program = &literal[..end];
        let last = program.rsplit('/').next().unwrap_or(program);
        if HARNESS.contains(&last) {
            hits.push(program.to_string());
        }
    }
    hits
}

/// Lines that print a skip notice: the signature of a test that returns
/// early and passes having checked nothing.
fn announced_skips(text: &str) -> Vec<String> {
    let print = ["eprint", "ln!("].concat();
    let lines: Vec<&str> = text.lines().collect();
    let mut hits = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !line.contains(&print) {
            continue;
        }
        // The message may sit on the next line or two after rustfmt.
        let window = lines[i..lines.len().min(i + 3)].join(" ").to_lowercase();
        if window.contains("skip") {
            hits.push(format!("line {}: {}", i + 1, line.trim()));
        }
    }
    hits
}

#[test]
fn no_test_runs_an_oracle_tool_on_the_host() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in direct_tool_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these run an oracle tool on the HOST. The tools live in the harness VM and \
         nowhere else: use common::oracle, which runs them there and fails, naming \
         the task that fixes it, when it cannot:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_announces_a_skip() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in announced_skips(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these print a skip notice. A test never skips on a missing tool, fixture or \
         VM; fail instead, naming the task that provides it:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_drives_the_vm_elevates_or_mounts_a_filesystem_itself() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in harness_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these drive the VM, elevate, or mount a filesystem themselves. The guest is \
         reached through tests/common (kernel_run, guest_script, oracle), which boots \
         it once per process and keeps one connection; a mount happens only inside the \
         guest, never on the host:\n{}",
        offenders.join("\n")
    );
}

#[test]
fn no_test_spawns_a_program_it_named_in_a_variable() {
    let mut offenders = Vec::new();
    for (path, text) in all_test_sources() {
        for hit in indirect_spawns(&text) {
            offenders.push(format!("{}: {hit}", path.display()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these spawn a program whose name is in a variable, which the checks above \
         cannot read. Only this crate's own binaries (CARGO_BIN_EXE_*) are spawned \
         that way; an oracle tool goes through common::oracle:\n{}",
        offenders.join("\n")
    );
}

/// The scans find what they are for, so the tests above cannot pass by
/// looking at nothing.
#[test]
fn the_scans_recognise_the_shapes_they_refuse() {
    let spawn = [
        "let out = Command",
        "::new(\"xfs_repair\").arg(img).output();\n",
        "let dbg = Command",
        "::new(\"/usr/sbin/xfs_db\");\n",
        "let ok = Command",
        "::new(tool).arg(img);\n",
        "let found = [\"/usr/",
        "sbin/xfs_repair\", \"/",
        "sbin/xfs_repair\"].into_iter().find(|p| exists(p));\n",
    ]
    .concat();
    let hits = direct_tool_spawns(&spawn);
    assert_eq!(hits.len(), 3, "{hits:?}");
    assert_eq!(
        hits[..2],
        ["xfs_repair".to_string(), "/usr/sbin/xfs_db".to_string()]
    );

    let indirect = [
        "const MKFS: &str = env!(\"CARGO_BIN",
        "_EXE_mkfs_xfs\");\n",
        "let out = Command",
        "::new(MKFS).output();\n",
        "let out = Command",
        "::new(tool).args(args).output();\n",
        "Command",
        "::new(\"sh\").arg(\"-c\");\n",
    ]
    .concat();
    assert_eq!(indirect_spawns(&indirect), ["tool".to_string()]);

    let harness = [
        "let vm = Command",
        "::new(\"../fs-linux-test-harness/scripts/vm.sh\");\n",
        "Command",
        "::new(\"sudo\").args([\"-n\", \"bash\"]);\n",
        "Command",
        "::new(\"mount\").args([\"-o\", \"loop\"]);\n",
        "Command",
        "::new(\"cargo\").arg(\"test\");\n",
    ]
    .concat();
    assert_eq!(
        harness_spawns(&harness),
        [
            "../fs-linux-test-harness/scripts/vm.sh".to_string(),
            "sudo".to_string(),
            "mount".to_string()
        ]
    );

    let skip = [
        "if missing {\n    eprint",
        "ln!(\n        \"SKIP: no image\"\n    );\n    return;\n}\n",
        "eprint",
        "ln!(\"note: took {ms} ms\");\n",
    ]
    .concat();
    assert_eq!(announced_skips(&skip).len(), 1);
}
