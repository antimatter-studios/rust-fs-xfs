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
//! `.vm-share` was unmounted cleanly and must mount as it stands.
//! `xfsdirty.img` was shut down mid-flight with `xfs_io -c shutdown` and
//! snapshotted while still mounted, so its log holds work that was never
//! applied — and since #90 that volume mounts too, by **replaying** the
//! log into memory rather than reading past it. What must never happen
//! is the third thing: the structures presented as they stand on disk,
//! silently, with the log's records ignored. The mount says which of the
//! two it did, and that is what this checks.
//!
//! The work in that fixture is deliberately renames, permission changes
//! and fresh allocations, and deliberately leaves nothing unlinked. That
//! is the case the previous check could not see: it inferred the log's
//! state from the AGI unlinked lists, so a filesystem interrupted in the
//! middle of any of this passed as clean.
//!
//! Both fixtures are gitignored and generated, and `chore fixtures`
//! builds both in the harness guest. A missing `xfsdirty.img` is that
//! build not having happened rather than a filesystem that came out
//! clean by chance, so it fails here and names the task: this is the
//! one suite whose whole claim rests on an image being dirty, and it
//! spent long enough passing with no image at all.

mod common;

use common::share;
use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::PathBuf;
use std::sync::Arc;

/// The crashed fixture, with the verdict `xfs_repair` gave it.
///
/// Both files come out of the same guest run, so either one missing is
/// the fixture build having failed and not a filesystem that happened to
/// shut down tidily.
fn dirty_fixture() -> (PathBuf, String) {
    let img = common::fixture("xfsdirty.img");
    let verdict = common::fixture("xfsdirty.verdict");
    let v = std::fs::read_to_string(&verdict)
        .unwrap_or_else(|e| panic!("reading {}: {e}", verdict.display()))
        .trim()
        .to_string();
    (img, v)
}

/// A filesystem whose log holds unapplied changes is replayed, and says
/// so (#90).
#[test]
fn a_dirty_log_is_replayed_rather_than_read_past() {
    let (img, verdict) = dirty_fixture();

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
    let fs = Filesystem::mount(Arc::new(dev))
        .expect("a volume whose log holds records mounts, replaying them");
    assert!(
        fs.was_replayed(),
        "xfs_repair says this volume's log holds unapplied records, and the mount says \
         it replayed nothing — so every structure it returns may be a version the log \
         was about to replace, and nothing would say so"
    );
    // And it reads: a replay that produced something unreadable would
    // otherwise pass the line above.
    let root = fs
        .root_inode()
        .expect("the root inode of the replayed volume");
    let raw = fs
        .read_inode_raw(root.ino)
        .expect("the root inode's record")
        .1;
    fs.read_dir(&root, &raw)
        .expect("the replayed volume's root directory lists");
}

/// And every cleanly unmounted fixture must still mount.
///
/// Without this the test above would pass on a driver that refused
/// everything, which is the easiest way to be accidentally correct about
/// dirty logs and useless about everything else.
#[test]
fn cleanly_unmounted_filesystems_still_mount() {
    let mut mounted = 0usize;
    for p in common::fixtures_matching("xfs", ".img") {
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
                "{name} was unmounted cleanly and a read-only mount still refused it \
                 for a dirty log, which no longer refuses anything (#90)"
            ),
            // Geometries this driver declines for other reasons are not
            // this test's business.
            Err(_) => {}
        }
    }
    // THE ONLY GUARD LEFT IN THIS TEST. The set itself can no longer be
    // empty — `common::fixtures_matching` fails first — but every image
    // in it could still be declined for some other reason, and the arm
    // that swallows those is deliberately broad. This count is what
    // says the check accepted something.
    assert!(
        mounted > 0,
        "no clean fixture mounted, so nothing here shows the check accepts anything"
    );
    eprintln!("{mounted} cleanly unmounted fixtures mounted");
}
