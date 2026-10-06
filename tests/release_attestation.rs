//! The release workflow attests the crate it publishes.
//!
//! A version on crates.io says nothing about where it was built: anyone
//! holding a publish token could have uploaded it from their own machine.
//! `release.yml` therefore packages the crate, publishes it, checks that
//! the file it packaged is byte-for-byte the one crates.io serves, and
//! signs a build-provenance attestation over that file with the
//! workflow's own identity. The same `.crate` is attached to the GitHub
//! release for the tag, so anyone can check a download with
//!
//! ```text
//! gh attestation verify <crate> --repo <owner>/<repo> \
//!     --signer-workflow <owner>/<repo>/.github/workflows/release.yml
//! ```
//!
//! Nothing else notices if that step goes. The workflow runs only on a
//! version tag, and a release without an attestation publishes exactly
//! as green as one with it; the gap would surface the first time someone
//! tried to verify a download, long after the version was taken. This
//! file makes the loss loud on the pull request that causes it.
//!
//! It also keeps the privileges where they are needed. The attesting job
//! must be able to mint an OIDC token, write an attestation and attach a
//! release asset; no other job, and not the workflow as a whole, may
//! hold any of those grants.
//!
//! The command-line tools' tarballs are packaged, attested and attached by
//! rust-fs-core's release-cli workflow, which this one calls
//! ([`core_call_gaps`]); this repository keeps no copy of those jobs.
//!
//! The workflow is PARSED rather than scanned, so a step name, a comment
//! or a quoted string cannot satisfy a check meant for a real step.

use saphyr::{LoadableYamlNode, Yaml};
use std::path::Path;

const WORKFLOW: &str = ".github/workflows/release.yml";

/// The action that signs the attestation, up to its `@`.
const ATTEST: &str = "actions/attest-build-provenance@";

/// The grants the attesting job needs, each at `write`: an OIDC token
/// to sign with, the attestation store, and the release to attach to.
const GRANTS: &[&str] = &["id-token", "attestations", "contents"];

fn load(yaml: &str) -> Yaml<'static> {
    let mut docs = Yaml::load_from_str(yaml).expect("the workflow parses as YAML");
    assert_eq!(docs.len(), 1, "one YAML document");
    docs.remove(0)
}

fn workflow() -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(WORKFLOW);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {WORKFLOW}: {e}"))
}

/// The lines of a `run:` script that are commands, not comments.
fn commands(step: &Yaml) -> Vec<String> {
    let Some(run) = step.as_mapping_get("run").and_then(Yaml::as_str) else {
        return Vec::new();
    };
    run.replace("\\\n", " ")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect()
}

/// The commands in a step that start with `program`, so an `echo` or
/// a string mentioning it does not count.
fn invocations(step: &Yaml, program: &str) -> Vec<String> {
    commands(step)
        .into_iter()
        .filter(|c| c.starts_with(program))
        .collect()
}

fn runs(step: &Yaml, program: &str) -> bool {
    !invocations(step, program).is_empty()
}

/// Every grant in `permissions` that is `write`, by name. `write-all`
/// grants every one.
fn write_grants(permissions: Option<&Yaml>) -> Vec<String> {
    let Some(permissions) = permissions else {
        return Vec::new();
    };
    if permissions.as_str() == Some("write-all") {
        return GRANTS.iter().map(|g| (*g).to_owned()).collect();
    }
    let Some(map) = permissions.as_mapping() else {
        return Vec::new();
    };
    map.iter()
        .filter(|(_, v)| v.as_str() == Some("write"))
        .filter_map(|(k, _)| k.as_str().map(str::to_owned))
        .filter(|k| GRANTS.contains(&k.as_str()))
        .collect()
}

fn is_full_sha(pin: &str) -> bool {
    pin.len() == 40
        && pin
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Everything wrong with how `yaml` attests what it publishes; empty
/// when nothing is.
fn attestation_gaps(yaml: &str) -> Vec<String> {
    let doc = load(yaml);
    let mut gaps = Vec::new();
    for grant in write_grants(doc.as_mapping_get("permissions")) {
        gaps.push(format!(
            "the workflow-level permissions grant {grant}: write to every job"
        ));
    }
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let mut attesting = 0;
    for (name, job) in jobs {
        // The tools' tarballs are attested inside rust-fs-core's
        // release-cli workflow, which the calling job holds the grants
        // for; `core_call_gaps` holds that call to its own rules.
        if job
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .is_some_and(|u| u.starts_with(CORE_RELEASE_CLI))
        {
            continue;
        }
        let name = name.as_str().unwrap_or("?");
        let steps: Vec<&Yaml> = job
            .as_mapping_get("steps")
            .and_then(Yaml::as_sequence)
            .map(|s| s.iter().collect())
            .unwrap_or_default();
        let granted = write_grants(job.as_mapping_get("permissions"));
        let attest_at = steps.iter().position(|s| {
            s.as_mapping_get("uses")
                .and_then(Yaml::as_str)
                .is_some_and(|u| u.starts_with(ATTEST))
        });
        let Some(at) = attest_at else {
            for grant in granted {
                gaps.push(format!(
                    "job {name} attests nothing but holds {grant}: write"
                ));
            }
            continue;
        };
        attesting += 1;
        let step = steps[at];
        let uses = step
            .as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .unwrap_or("");
        let pin = &uses[ATTEST.len()..];
        if !is_full_sha(pin) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let subject = step
            .as_mapping_get("with")
            .and_then(|w| w.as_mapping_get("subject-path"))
            .and_then(Yaml::as_str)
            .unwrap_or("");
        if !subject.contains(".crate") {
            gaps.push(format!(
                "job {name} attests {subject:?}, not the packaged .crate"
            ));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo package")) {
            gaps.push(format!("job {name} attests before any `cargo package`"));
        }
        if !steps[..at].iter().any(|s| runs(s, "cargo publish")) {
            gaps.push(format!(
                "job {name} attests before `cargo publish`, so what it signs is not \
                 known to be what was published"
            ));
        }
        if !steps[at + 1..].iter().any(|s| {
            invocations(s, "gh release upload")
                .iter()
                .any(|c| c.contains(".crate"))
        }) {
            gaps.push(format!(
                "job {name} does not attach the attested .crate to the GitHub release"
            ));
        }
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!("job {name} attests without {grant}: write"));
            }
        }
    }
    if attesting == 0 {
        gaps.push(format!("no job in the workflow uses {ATTEST}<sha>"));
    }
    gaps
}

/// The steps of a job.
fn steps_of<'a>(job: &'a Yaml<'a>) -> Vec<&'a Yaml<'a>> {
    job.as_mapping_get("steps")
        .and_then(Yaml::as_sequence)
        .map(|s| s.iter().collect())
        .unwrap_or_default()
}

/// Where a job's attestation step is, and what it attests.
fn attest_step<'a>(job: &'a Yaml<'a>) -> Option<(usize, &'a str, String)> {
    let steps = steps_of(job);
    let at = steps.iter().position(|s| {
        s.as_mapping_get("uses")
            .and_then(Yaml::as_str)
            .is_some_and(|u| u.starts_with(ATTEST))
    })?;
    let uses = steps[at]
        .as_mapping_get("uses")
        .and_then(Yaml::as_str)
        .unwrap_or("");
    let subject = steps[at]
        .as_mapping_get("with")
        .and_then(|w| w.as_mapping_get("subject-path"))
        .and_then(Yaml::as_str)
        .unwrap_or("")
        .to_string();
    Some((at, uses, subject))
}

/// Whether a job attests the command-line tools' release tarballs.
fn attests_tarballs(job: &Yaml) -> bool {
    attest_step(job).is_some_and(|(_, _, subject)| subject.contains(".tar.gz"))
}

#[test]
fn the_release_workflow_attests_the_crate_it_publishes() {
    let gaps = attestation_gaps(&workflow());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must attest the .crate it publishes, from the one job that \
         publishes it, with only that job privileged: {gaps:#?}"
    );
}

/// The reader answers for the inputs it is meant to catch, and not for
/// the ones it is not.
#[test]
fn the_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         jobs:\n  test:\n    steps:\n      - run: cargo test\n\
         \x20 publish:\n    permissions:\n      id-token: write\n      attestations: write\n      contents: write\n\
         \x20   steps:\n      - run: cargo package --no-verify\n      - run: cargo publish\n\
         \x20     - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n\
         \x20     - run: gh release upload \"$GITHUB_REF_NAME\" target/package/*.crate --clobber\n"
    );
    assert_eq!(attestation_gaps(&good), Vec::<String>::new(), "{good}");

    let expect = |yaml: String, want: &str| {
        let gaps = attestation_gaps(&yaml);
        assert!(
            gaps.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {gaps:#?} for\n{yaml}"
        );
    };
    // The step gone entirely, or only named in a comment.
    let no_step = good.replace(&format!("      - uses: {ATTEST}{sha} # v4.2.2\n        with:\n          subject-path: target/package/*.crate\n"), "      # uses: actions/attest-build-provenance\n");
    expect(no_step, "no job in the workflow uses");
    // Pinned to a tag.
    expect(good.replace(sha, "v4.2.2"), "pin a full commit SHA");
    // Each grant dropped in turn.
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("attests without {grant}: write"),
        );
    }
    // A grant hoisted to the whole workflow.
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions:\n  id-token: write\n",
        ),
        "workflow-level permissions grant id-token",
    );
    expect(
        good.replace(
            "permissions:\n  contents: read\n",
            "permissions: write-all\n",
        ),
        "workflow-level permissions grant attestations",
    );
    // A job that attests nothing, holding a grant.
    expect(
        good.replace(
            "  test:\n    steps:",
            "  test:\n    permissions:\n      id-token: write\n    steps:",
        ),
        "job test attests nothing but holds id-token: write",
    );
    // Signing before publishing, or something other than the crate.
    expect(
        good.replace("      - run: cargo publish\n", "")
            .replace("--clobber\n", "--clobber\n      - run: cargo publish\n"),
        "attests before `cargo publish`",
    );
    expect(
        good.replace(
            "subject-path: target/package/*.crate",
            "subject-path: Cargo.toml",
        ),
        "not the packaged .crate",
    );
    expect(
        good.replace(
            "      - run: cargo package --no-verify\n",
            "      - run: echo '# cargo package'\n",
        ),
        "attests before any `cargo package`",
    );
    // Not attached to the release.
    expect(
        good.replace("gh release upload", "echo gh-release-upload"),
        "does not attach the attested .crate",
    );
}

/// The reusable workflow in rust-fs-core that packages, attests and
/// attaches the command-line tools' tarballs, up to its `@`.
///
/// It is the family's one copy of that job. This repository carried its
/// own -- a `package-cli` matrix, an attest-and-attach job and
/// `scripts/package-cli.sh` -- and ten such copies had already drifted
/// (rust-fs-core#193). The call is checked here because nothing else
/// would notice it going: the workflow runs only on a version tag.
const CORE_RELEASE_CLI: &str = "antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml@";

fn repo_file(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The toolchain `rust-toolchain.toml` pins.
fn pinned_toolchain() -> String {
    repo_file("rust-toolchain.toml")
        .lines()
        .find_map(|l| {
            let v = l.trim().strip_prefix("channel")?.trim().strip_prefix('=')?;
            Some(v.trim().trim_matches('"').to_owned())
        })
        .expect("rust-toolchain.toml pins a channel")
}

/// The rust-fs-core version `Cargo.toml` depends on.
fn manifest_core_version() -> String {
    repo_file("Cargo.toml")
        .lines()
        .find(|l| l.starts_with("rust-fs-core = "))
        .and_then(|l| l.split("version = \"").nth(1))
        .and_then(|v| v.split('"').next())
        .map(str::to_owned)
        .expect("Cargo.toml depends on rust-fs-core with a version")
}

/// Everything wrong with how `yaml` hands the tools' tarballs to
/// rust-fs-core's release-cli workflow; empty when nothing is.
///
/// One job calls it, pinned by commit SHA with the tag beside it as a
/// comment, after the gate and after `publish` (the one job that creates
/// the release), holding the three grants the called `attach` job needs,
/// with `core-ref` the tag `FS_CORE_REF` and the manifest name and
/// `toolchain` the one `rust-toolchain.toml` pins. No other job packages
/// or attests a tarball of its own.
fn core_call_gaps(yaml: &str, toolchain: &str, core_version: &str) -> Vec<String> {
    let doc = load(yaml);
    let jobs = doc
        .as_mapping_get("jobs")
        .and_then(Yaml::as_mapping)
        .expect("the workflow has jobs");
    let core_ref = doc
        .as_mapping_get("env")
        .and_then(|e| e.as_mapping_get("FS_CORE_REF"))
        .and_then(Yaml::as_str)
        .unwrap_or("");
    let mut gaps = Vec::new();
    if core_ref != format!("v{core_version}") {
        gaps.push(format!(
            "FS_CORE_REF is {core_ref:?}, not v{core_version}, the rust-fs-core Cargo.toml depends on"
        ));
    }
    let mut calls = 0;
    for (name, job) in jobs {
        let name = name.as_str().unwrap_or("?");
        for step in steps_of(job) {
            if commands(step).iter().any(|c| c.contains("package-cli.sh")) {
                gaps.push(format!(
                    "job {name} packages the tools with a local package-cli.sh"
                ));
            }
        }
        if attests_tarballs(job) {
            gaps.push(format!("job {name} attests the tools' tarballs itself"));
        }
        let Some(uses) = job.as_mapping_get("uses").and_then(Yaml::as_str) else {
            continue;
        };
        let Some(pin) = uses.strip_prefix(CORE_RELEASE_CLI) else {
            continue;
        };
        calls += 1;
        if !is_full_sha(pin) {
            gaps.push(format!(
                "job {name} uses {uses}, which a moved tag can redirect; pin a full commit SHA"
            ));
        }
        let commented = yaml
            .lines()
            .any(|l| l.trim() == format!("uses: {uses} # {core_ref}"));
        if !commented {
            gaps.push(format!(
                "job {name} does not name the tag of its pin as `uses: {uses} # {core_ref}`"
            ));
        }
        let with = job.as_mapping_get("with");
        let input = |key: &str| {
            with.and_then(|w| w.as_mapping_get(key))
                .and_then(Yaml::as_str)
                .unwrap_or("")
                .to_owned()
        };
        if input("core-ref") != core_ref {
            gaps.push(format!(
                "job {name} passes core-ref {:?}, not FS_CORE_REF {core_ref:?}",
                input("core-ref")
            ));
        }
        if input("toolchain") != toolchain {
            gaps.push(format!(
                "job {name} passes toolchain {:?}, not {toolchain:?} from rust-toolchain.toml",
                input("toolchain")
            ));
        }
        let needs: Vec<&str> = match job.as_mapping_get("needs") {
            Some(n) if n.as_str().is_some() => n.as_str().into_iter().collect(),
            Some(n) => n
                .as_sequence()
                .map(|s| s.iter().filter_map(Yaml::as_str).collect())
                .unwrap_or_default(),
            None => Vec::new(),
        };
        for need in ["gate", "publish"] {
            if !needs.contains(&need) {
                gaps.push(format!("job {name} does not wait for {need}"));
            }
        }
        let granted = write_grants(job.as_mapping_get("permissions"));
        for grant in GRANTS {
            if !granted.iter().any(|g| g == grant) {
                gaps.push(format!(
                    "job {name} calls release-cli without {grant}: write"
                ));
            }
        }
    }
    if calls != 1 {
        gaps.push(format!(
            "{calls} jobs call {CORE_RELEASE_CLI}<sha>; the tools' tarballs need exactly one"
        ));
    }
    gaps
}

#[test]
fn the_release_workflow_packages_the_tools_through_core() {
    let gaps = core_call_gaps(&workflow(), &pinned_toolchain(), &manifest_core_version());
    assert!(
        gaps.is_empty(),
        "{WORKFLOW} must hand the tools' tarballs to rust-fs-core's release-cli workflow: \
         {gaps:#?}"
    );
}

/// No copy of the packaging is left to drift, and what the tarball ships
/// is declared where the shared script reads it.
#[test]
fn this_repository_keeps_no_copy_of_the_packaging() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for copy in [
        "scripts/package-cli.sh",
        "tests/scripts/test-package-cli.sh",
    ] {
        assert!(
            !root.join(copy).exists(),
            "{copy} is a copy of rust-fs-core's packaging; `../rust-fs-core/scripts/package-cli.sh` runs core's"
        );
    }
    assert!(
        repo_file("Cargo.toml").contains("\n[package.metadata.package-cli]\n"),
        "Cargo.toml must declare [package.metadata.package-cli], which core's package-cli.sh reads"
    );
}

/// The core-call reader answers for the shapes it is meant to catch.
#[test]
fn the_core_call_reader_discriminates() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let good = format!(
        "permissions:\n  contents: read\n\
         env:\n  FS_CORE_REF: v0.2.23\n\
         jobs:\n  gate:\n    uses: ./.github/workflows/ci.yml\n\
         \x20 publish:\n    steps:\n      - run: cargo publish\n\
         \x20 cli:\n    needs: [gate, publish]\n\
         \x20   permissions:\n      contents: write\n      id-token: write\n      attestations: write\n\
         \x20   uses: {CORE_RELEASE_CLI}{sha} # v0.2.23\n\
         \x20   with:\n      core-ref: v0.2.23\n      toolchain: 1.95.0\n"
    );
    let gaps = |yaml: &str| core_call_gaps(yaml, "1.95.0", "0.2.23");
    assert_eq!(gaps(&good), Vec::<String>::new(), "{good}");
    let expect = |yaml: String, want: &str| {
        let got = gaps(&yaml);
        assert!(
            got.iter().any(|g| g.contains(want)),
            "expected a gap mentioning {want:?}, got {got:#?} for\n{yaml}"
        );
    };
    expect(
        good.replace("release-cli.yml@", "release-tools.yml@"),
        "0 jobs call",
    );
    expect(good.replace(sha, "v0.2.23"), "pin a full commit SHA");
    expect(
        good.replace(&format!("{sha} # v0.2.23"), sha),
        "does not name the tag",
    );
    expect(
        good.replace("core-ref: v0.2.23", "core-ref: v0.2.18"),
        "passes core-ref",
    );
    expect(
        good.replace("FS_CORE_REF: v0.2.23", "FS_CORE_REF: v0.2.18"),
        "not v0.2.23",
    );
    expect(
        good.replace("toolchain: 1.95.0", "toolchain: stable"),
        "passes toolchain",
    );
    expect(
        good.replace("needs: [gate, publish]", "needs: [gate]"),
        "does not wait for publish",
    );
    for grant in GRANTS {
        expect(
            good.replace(&format!("      {grant}: write\n"), ""),
            &format!("calls release-cli without {grant}: write"),
        );
    }
    expect(
        good.replace(
            "      - run: cargo publish\n",
            "      - run: scripts/package-cli.sh 1.0.0 linux-x86_64\n",
        ),
        "local package-cli.sh",
    );
    expect(
        good.replace(
            "      - run: cargo publish\n",
            &format!("      - uses: {ATTEST}{sha}\n        with:\n          subject-path: dist/*.tar.gz\n"),
        ),
        "attests the tools' tarballs itself",
    );
}
