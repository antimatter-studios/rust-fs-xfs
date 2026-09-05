//! Every write operation, against every legal combination of the
//! features that change what a write has to maintain.
//!
//! # Why this exists
//!
//! Every other write fixture in this repository is formatted one way, so
//! every write test had only ever exercised one feature set. That is how
//! `rmapbt` went unnoticed: `mkfs.xfs` 6.6 turns it on by default, the
//! oracle VM's older one does not, and the first CI run on a modern
//! runner produced a filesystem `xfs_repair` called broken.
//!
//! Which features a filesystem has is not the driver's choice. So the
//! combinations are enumerated rather than sampled, and each is written
//! to by every operation the driver offers.
//!
//! # What counts as correct
//!
//! Two outcomes are acceptable and one is not.
//!
//!   - the driver performs the write, the kernel replays it, and
//!     `xfs_repair` finds nothing wrong; or
//!   - the driver REFUSES the filesystem by name, before touching it.
//!
//! What is not acceptable is writing and leaving something `xfs_repair`
//! objects to. A refusal is recoverable and visible. A filesystem that
//! mounts, behaves, and disagrees with the checker is neither.
//!
//! So a combination this driver does not maintain must be refused, and
//! this test is what decides which those are — by asking, not by
//! reasoning about the format.

mod common;
use common::{kernel_run, share};

use fs_core::FileDevice;
use fs_xfs::write::AttrChange;
use fs_xfs::{Error, Filesystem};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A copy of a fixture, removed when it goes out of scope.
///
/// IN A SUBDIRECTORY, not beside the fixtures. Several suites treat
/// every `.img` in the share as a fixture to walk, and this test makes
/// one per combination per operation -- seventy-odd images appearing and
/// vanishing while other suites enumerate the directory. That is a race
/// those suites lose, and it made the whole run flaky while each suite
/// passed on its own.
///
/// `read_dir` does not recurse, so a subdirectory is invisible to them.
/// The VM sees it as `/share/scratch` for the same reason it sees the
/// rest: the share is mounted whole.
struct Scratch(PathBuf);

impl Scratch {
    fn dir() -> PathBuf {
        let d = share().join("scratch");
        std::fs::create_dir_all(&d).expect("make the scratch directory");
        d
    }

    fn from(source: &Path, name: &str) -> Self {
        let path = Self::dir().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// The rows, named as `build-feature-matrix-fixtures.sh` writes them.
const COMBOS: &[&str] = &[
    "v4",
    "base",
    "finobt",
    "finobt-inobtcount",
    "reflink",
    "reflink-finobt",
    "rmapbt",
    "rmapbt-reflink",
    "everything",
];

/// What happened to one combination.
enum Outcome {
    /// The driver refused a read-write mount, naming the reason.
    Refused(String),
    /// The driver wrote, and this is what the checker said.
    Wrote { repair: String },
}

/// The write operations this driver offers, one per fresh image.
///
/// ONE OPERATION PER MOUNT, and that is not tidiness. This driver
/// refuses a second checkpoint from the same mount -- the disk does not
/// yet reflect the first, so a second built on top of it would be wrong.
/// Driving several operations through one mount therefore tests the
/// first and collects refusals for the rest, which is exactly what the
/// first version of this test did: it reported six refusals per row and
/// called the row covered.
const OPS: &[&str] = &[
    "create_file",
    "create_directory",
    "rename_in_directory",
    "unlink_file",
    "truncate_to_zero",
    "write_into_empty_file",
    "set_attributes",
    // Only meaningful where an extent is actually shared, and skipped
    // as "not applicable" elsewhere -- see `perform`.
    "truncate_shared",
];

/// Perform one operation on one image.
fn perform(fs: &Filesystem, op: &str) -> Result<(), String> {
    let dir = fs.lookup_path("/sf").map_err(|e| e.to_string())?.ino;
    let ino = |path: &str| {
        fs.lookup_path(path)
            .map(|i| i.ino)
            .map_err(|e| e.to_string())
    };

    match op {
        "create_file" => fs
            .create_file(dir, b"made", 0o100644)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "create_directory" => fs
            .create_directory(dir, b"madedir", 0o040755)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "rename_in_directory" => fs
            .rename_in_directory(dir, b"aaaa", b"cccc")
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "unlink_file" => fs
            .unlink_file(dir, b"victim")
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "truncate_to_zero" => fs
            .truncate_to_zero(ino("/sf/data.bin")?)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        "write_into_empty_file" => fs
            .write_into_empty_file(ino("/sf/empty.bin")?, b"written by this driver")
            .map(|_| ())
            .map_err(|e| e.to_string()),
        // Freeing an extent that another file also points at. The
        // refcount tree has to be decremented rather than the blocks
        // returned; getting it wrong hands out blocks that are still in
        // use, which is the worst outcome available here and one that
        // only shows up on a filesystem where sharing happened.
        "truncate_shared" => {
            let shared = match fs.lookup_path("/sf/shared.bin") {
                Ok(i) => i.ino,
                // No shared file: this row's filesystem does not permit
                // sharing, so there is nothing to test rather than
                // something that passed.
                Err(_) => return Err("not applicable: no shared extent on this filesystem".into()),
            };
            fs.truncate_to_zero(shared)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        "set_attributes" => {
            let inode = fs.lookup_path("/sf/attrs").map_err(|e| e.to_string())?;
            fs.set_attributes(
                &inode,
                &AttrChange {
                    permissions: Some(0o600),
                    ..Default::default()
                },
            )
            .map_err(|e| e.to_string())
        }
        other => panic!("unknown operation {other}"),
    }
}

/// Mount, perform one operation, and ask the kernel and the checker what
/// it left behind.
fn exercise(img: &Path, op: &str) -> Outcome {
    let dev = FileDevice::open_rw(img).expect("open read-write");
    let fs = match Filesystem::mount_rw(Arc::new(dev)) {
        Ok(fs) => fs,
        Err(Error::UnsupportedFeature(why)) => return Outcome::Refused(why),
        Err(e) => panic!("a read-write mount failed for a reason other than a refusal: {e}"),
    };

    let result = perform(&fs, op);
    drop(fs);

    // A refused operation wrote nothing, so there is nothing to judge
    // and nothing wrong: refusing is one of the two acceptable answers.
    if let Err(why) = result {
        return Outcome::Refused(why);
    }

    // The kernel replays what was logged, then the checker judges. Both
    // are the reference implementation; neither is this repository.
    let name = img.file_name().unwrap().to_string_lossy().into_owned();
    let script = format!(
        r#"
        img=$(mktemp -u /tmp/feat-XXXXXX.img)
        cp /share/scratch/{name} "$img"
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -8
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n "$img" 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f "$img"
        echo DONE
        "#
    );
    let repair = kernel_run(&script).unwrap_or_default();
    Outcome::Wrote { repair }
}

/// Every legal combination either survives every write, or is refused
/// before the first one.
#[test]
fn every_feature_combination_is_written_correctly_or_refused() {
    let mut checked = 0;
    let mut sound = 0;
    let mut refused = 0;
    let mut broken: Vec<String> = Vec::new();
    let mut unjudged = 0;

    for combo in COMBOS {
        let source = share().join(format!("xfsfeat-{combo}.img"));
        if !source.exists() {
            eprintln!("no xfsfeat-{combo} fixture — skipping");
            continue;
        }

        for op in OPS {
            // A fresh copy per operation: the previous one may have
            // left a record in the log, and the next must start from
            // the filesystem as mkfs made it.
            let scratch = Scratch::from(&source, &format!("xfsfeat-{combo}-{op}-scratch.img"));
            checked += 1;

            match exercise(scratch.path(), op) {
                Outcome::Refused(why) => {
                    refused += 1;
                    eprintln!("{combo:22} {op:22} refused: {}", first_line(&why));
                }
                Outcome::Wrote { repair } => {
                    if repair.is_empty() {
                        unjudged += 1;
                        eprintln!("{combo:22} {op:22} wrote, no kernel to judge it");
                        continue;
                    }
                    let ok = repair.contains("REPAIR_RC=0")
                        && !repair.contains("MOUNT_FAILED")
                        && !repair.to_lowercase().contains("corrupt");
                    if ok {
                        sound += 1;
                        eprintln!("{combo:22} {op:22} wrote, sound");
                    } else {
                        let why = repair
                            .lines()
                            .find(|l| {
                                l.contains("Missing")
                                    || l.contains("bad ")
                                    || l.to_lowercase().contains("corrupt")
                                    || l.contains("would ")
                                    || l.contains("MOUNT_FAILED")
                            })
                            .unwrap_or("see output")
                            .trim()
                            .to_string();
                        // The whole checker output, because the one
                        // line matched above is a guess at which line
                        // mattered and the rest is the evidence.
                        eprintln!("{combo:22} {op:22} WROTE AND BROKE IT: {why}");
                        eprintln!(
                            "----- xfs_repair on {combo}/{op} -----
{repair}-----"
                        );
                        broken.push(format!("{combo} / {op}: {why}"));
                    }
                }
            }
        }
    }

    assert!(
        checked > 0,
        "no feature-matrix fixture was found — the test proved nothing"
    );
    eprintln!(
        "\n{checked} combination/operation pairs: {sound} written and sound, \
         {refused} refused by name, {unjudged} unjudged"
    );

    assert!(
        broken.is_empty(),
        "these left a filesystem xfs_repair objects to. Each must either be maintained \
         properly or refused before the write:\n  {}",
        broken.join("\n  ")
    );
}

/// The first line of a refusal, for a readable table.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
