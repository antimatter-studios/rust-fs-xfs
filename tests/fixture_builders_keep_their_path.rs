//! A fixture builder runs its tools as root **with this user's PATH**
//! (#221).
//!
//! `sudo` replaces PATH with its own `secure_path`. An xfsprogs
//! installed for this user — under `~/.local/bin`, or a wrapper that
//! finds its binary through `$HOME` — is then simply not there when the
//! command runs as root, and the tool reports "command not found" from a
//! script nobody is watching.
//!
//! What that produces is not a failed build. It is a fixture that was
//! built, is the wrong shape, and says nothing:
//!
//! - #211: `xfs_io -c 'shutdown -f'` was not found, its failure was
//!   swallowed by `|| true`, and the crashed-log fixture came out with a
//!   clean log — a healthy volume pretending to be a crashed one;
//!
//! - #221: `xfs_bmap` was not found, so the builder could not see where
//!   any file had landed, every neighbour read as "none", and all four
//!   truncate cases were built as the same one. The suite reported it
//!   three steps later as a driver fault, and it was chased as one.
//!
//! So every builder carries PATH and HOME through, in one helper with
//! one name, and this is what says so.

use std::path::{Path, PathBuf};

fn scripts() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts")
}

/// The shape that loses the PATH: `SUDO="sudo"` and `$SUDO tool`.
#[test]
fn no_builder_runs_a_tool_through_a_bare_sudo() {
    let mut bare = Vec::new();
    for entry in std::fs::read_dir(scripts()).expect("the scripts directory") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // The builders, which run tools installed for this user. `vm.sh`
        // says `sudo` too, inside the guest, where root's PATH is the
        // one that matters and the tools are the system's.
        if !name.starts_with("build-") || !name.ends_with(".sh") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a script");
        let offending: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter(|l| {
                // The helper's own line is the one place `sudo` is named.
                l.contains("$SUDO")
                    || l.contains(r#"SUDO="sudo""#)
                    || (l.contains("sudo ") && !l.contains("sudo env PATH="))
            })
            .collect();
        if !offending.is_empty() {
            bare.push(format!("{name}: {}", offending.join(" | ")));
        }
    }
    assert!(
        bare.is_empty(),
        "these scripts run something as root without carrying PATH through, so a tool \
         installed for this user is not found and the fixture is built wrong rather \
         than not at all:\n{}",
        bare.join("\n")
    );
}

/// And the helper that replaces it is the same one everywhere.
#[test]
fn every_builder_that_needs_root_defines_the_same_helper() {
    let mut missing = Vec::new();
    for entry in std::fs::read_dir(scripts()).expect("the scripts directory") {
        let path = entry.expect("an entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("build-") || !name.ends_with(".sh") {
            continue;
        }
        let body = std::fs::read_to_string(&path).expect("a script");
        // A builder that mounts anything needs root; one that only calls
        // mkfs on a file does not.
        //
        // WHAT IT RUNS, NOT WHAT IT SAYS, which is the same rule the
        // scan above already follows. `scripts/build-fixtures.sh` drives
        // the build from the host and mounts nothing — the guest does
        // the mounting — but it explains the repository mount in prose,
        // and reading the prose put it here, demanding a root helper
        // that would have nothing to do.
        let mounts = body
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .any(|l| l.contains("mount "));
        if !mounts {
            continue;
        }
        if !body.contains("as_root()") || !body.contains("sudo env PATH=") {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "these builders mount filesystems without the as_root helper, so how they \
         reach root is theirs to get wrong: {missing:?}"
    );
}
