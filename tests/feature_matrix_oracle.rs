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
    // How things are encoded, with the features held still.
    "bigtime0",
    "nrext64",
    "nrext64-bigtime0",
    "sparse",
    "nosparse",
    "b1k",
    "b2k",
    "i1k",
    "dirblock8k",
    "ci",
    "fullinodes",
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
    // The only operation here that allocates for a directory.
    "convert_directory",
    // An operation in an allocation group above the first.
    "create_in_later_group",
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
        // Creating in a directory that lives in a group above the
        // first. Every arithmetic mistake this driver has made about
        // block numbers was invisible in group 0 -- a packed fsbno and
        // a linear block number are the same value there -- so an
        // operation that never leaves it cannot catch the next one.
        "create_in_later_group" => {
            let sb = fs.superblock();
            let spread = fs.lookup_path("/spread").map_err(|e| e.to_string())?;
            let (inode, raw) = fs.read_inode_raw(spread.ino).map_err(|e| e.to_string())?;
            let later = fs
                .read_dir(&inode, &raw)
                .map_err(|e| e.to_string())?
                .into_iter()
                .filter(|e| e.name != b"." && e.name != b"..")
                .map(|e| e.ino)
                .find(|ino| sb.split_ino(*ino).0 > 0)
                .ok_or_else(|| {
                    "not applicable: no directory landed in a group above the first".to_string()
                })?;
            fs.create_file(later, b"faraway", 0o100644)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        // Adding an entry to a directory that has no room left, which
        // moves it out of the inode and into a block of its own. That
        // block has to be allocated, so this is the one operation here
        // that takes space for a directory -- and the one that a
        // filesystem whose directory block is larger than its
        // filesystem block cannot do.
        "convert_directory" => {
            let full = fs.lookup_path("/full").map_err(|e| e.to_string())?.ino;
            fs.create_file(full, b"overflow", 0o100644)
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
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

        # MOUNT, CHECK, AND RETRY IF THE LOG DID NOT GO IN.
        #
        # Mounting replays the log. When it does not -- and it
        # occasionally does not -- xfs_repair describes an unreplayed log
        # rather than anything this driver wrote, and says so itself:
        # "valuable metadata changes in a log which is being ignored ...
        # Expect spurious inconsistencies".
        #
        # Mounting twice up front made that rarer and not rare enough: a
        # CI run still came back with one pair unjudged. An unjudged pair
        # is a measurement nobody took, so this keeps taking it until the
        # log is in, rather than shrugging.
        mounted=0
        for attempt in 1 2 3; do
            if mount -o loop,nouuid "$img" "$m"; then
                umount "$m"
                mounted=$((mounted + 1))
            fi
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            case "$out" in
                *"valuable metadata changes in a log"*) continue ;;
                *) break ;;
            esac
        done
        rmdir "$m" 2>/dev/null
        [ "$mounted" -gt 0 ] || {{ echo "MOUNT_FAILED"; dmesg | tail -8; }}
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
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
    let mut not_applicable = 0;
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
                    // "Not applicable" is the test saying this row has
                    // nothing to exercise -- a filesystem that cannot
                    // share extents has no shared extent to free. That
                    // is not the driver declining to do something, and
                    // counting the two together overstates how much is
                    // refused.
                    if why.starts_with("not applicable") {
                        not_applicable += 1;
                        eprintln!("{combo:22} {op:22} n/a: {}", first_line(&why));
                    } else {
                        refused += 1;
                        eprintln!("{combo:22} {op:22} refused: {}", first_line(&why));
                    }
                }
                Outcome::Wrote { repair } => {
                    if repair.is_empty() {
                        unjudged += 1;
                        eprintln!("{combo:22} {op:22} wrote, no kernel to judge it");
                        continue;
                    }
                    // A KERNEL THAT REFUSED THE IMAGE IS A FAILURE, and
                    // it has to be tested for FIRST. A refused mount
                    // leaves the log unreplayed, so the check below
                    // matches too — and when it came first it turned the
                    // worst possible result into "not judged". That is
                    // how a filesystem the kernel shut down on sight was
                    // nearly recorded as untested rather than broken.
                    if repair.contains("MOUNT_FAILED") {
                        eprintln!("{combo:22} {op:22} THE KERNEL REFUSED IT\n{repair}");
                        broken.push(format!("{combo} / {op}: the kernel refused to mount it"));
                        continue;
                    }

                    // The checker disqualifies its own answer when the
                    // log was not replayed, and says so. Counting that
                    // as a verdict on this driver would be reading a
                    // measurement the instrument called spurious.
                    if repair.contains("valuable metadata changes in a log") {
                        unjudged += 1;
                        eprintln!(
                            "{combo:22} {op:22} NOT JUDGED: the log was not replayed, so \
                             xfs_repair is describing that rather than this driver"
                        );
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

    // NO FIXTURE AT ALL IS A FRESH CHECKOUT, not a failure. That is the
    // contract every suite here keeps, and the job that builds the
    // fixtures runs through scripts/ci-test.sh, which fails on a skip --
    // so this cannot go quiet where it matters.
    //
    // An assertion here instead of a skip is what broke the no-fixture
    // job: it has no fixtures on purpose.
    if checked == 0 {
        eprintln!(
            "no feature-matrix fixtures — skipping. Build them with \
             `sudo ./scripts/build-feature-matrix-fixtures.sh` (needs xfsprogs, so Linux)."
        );
        return;
    }
    eprintln!(
        "\n{checked} combination/operation pairs: {sound} written and sound, \
         {refused} refused by name, {not_applicable} not applicable, {unjudged} unjudged"
    );

    // Unjudged is not the same as sound, and a run where several pairs
    // went unjudged has proved less than it looks like it has.
    assert!(
        unjudged * 4 < checked,
        "{unjudged} of {checked} pairs went unjudged — too many to call this run a check"
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
