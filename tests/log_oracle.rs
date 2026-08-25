//! Whether a log needs replaying is decided by `xfs_repair`, and this
//! driver has to agree with it.
//!
//! A filesystem that was not shut down cleanly holds metadata in its log
//! that the structures themselves have not seen. Reading it as though it
//! were current is the worst failure available to a read-only driver,
//! because there is no symptom: directories parse, checksums verify,
//! files read. The contents are simply stale, and a caller has no way to
//! notice.
//!
//! So there are two fixtures and both matter. Every other image in
//! `.vm-share` was unmounted cleanly and must mount — a check that
//! always answered "dirty" would be useless. `xfsdirty.img` was shut
//! down mid-flight with `xfs_io -c shutdown` and snapshotted while still
//! mounted, so its log holds work that was never applied, and it must be
//! refused.
//!
//! The work in that fixture is deliberately renames, permission changes
//! and fresh allocations, and deliberately leaves nothing unlinked. That
//! is the case the previous check could not see: it inferred the log's
//! state from the AGI unlinked lists, so a filesystem interrupted in the
//! middle of any of this passed as clean.
//!
//! Fixtures are gitignored, so this skips on a fresh clone. Generate
//! them with `./scripts/vm-build-data-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// The crashed fixture, with the verdict `xfs_repair` gave it.
fn dirty_fixture() -> Option<(PathBuf, String)> {
    let img = share().join("xfsdirty.img");
    let verdict = share().join("xfsdirty.verdict");
    if !img.exists() || !verdict.exists() {
        return None;
    }
    let v = std::fs::read_to_string(&verdict).ok()?.trim().to_string();
    Some((img, v))
}

/// A filesystem whose log holds unapplied changes must be refused.
#[test]
fn a_dirty_log_is_refused() {
    let Some((img, verdict)) = dirty_fixture() else {
        eprintln!("no xfsdirty fixture in .vm-share — skipping");
        return;
    };

    // The fixture only tests anything if the reference tool agrees it is
    // dirty. A kernel or xfsprogs that shut the filesystem down more
    // tidily would leave a clean log here, and this suite would pass
    // while checking nothing.
    assert_eq!(
        verdict,
        "DIRTY",
        "xfs_repair considers xfsdirty.img clean, so it is not a dirty-log fixture. \
         Its report:\n{}",
        std::fs::read_to_string(share().join("xfsdirty.repair")).unwrap_or_default()
    );

    let dev = FileDevice::open(&img).expect("open the crashed image");
    match Filesystem::mount(Arc::new(dev)) {
        Err(fs_xfs::Error::DirtyLog) => {}
        Ok(_) => panic!(
            "a filesystem xfs_repair says has unreplayed log records mounted as clean — \
             every structure it returns may be a version the log was about to replace"
        ),
        Err(other) => panic!("expected DirtyLog, got {other}"),
    }
}

/// And every cleanly unmounted fixture must still mount.
///
/// Without this the test above would pass on a driver that refused
/// everything, which is the easiest way to be accidentally correct about
/// dirty logs and useless about everything else.
#[test]
fn cleanly_unmounted_filesystems_still_mount() {
    let Ok(entries) = std::fs::read_dir(share()) else {
        eprintln!("no .vm-share — skipping");
        return;
    };

    let mut mounted = 0usize;
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let name = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        // The crashed fixture is the one image expected to refuse.
        if name.starts_with("xfsdirty") {
            continue;
        }
        let dev = FileDevice::open(&p).unwrap_or_else(|e| panic!("{name}: open: {e}"));
        match Filesystem::mount(Arc::new(dev)) {
            Ok(_) => mounted += 1,
            Err(fs_xfs::Error::DirtyLog) => panic!(
                "{name} was unmounted cleanly but the log check calls it dirty — \
                 the check is refusing filesystems it should accept"
            ),
            // Geometries this driver declines for other reasons are not
            // this test's business.
            Err(_) => {}
        }
    }
    assert!(
        mounted > 0,
        "no clean fixture mounted, so nothing here shows the check accepts anything"
    );
    eprintln!("{mounted} cleanly unmounted fixtures mounted");
}
