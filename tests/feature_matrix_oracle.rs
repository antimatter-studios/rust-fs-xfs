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
#[path = "common/feature_expectations.rs"]
mod contract;
use common::{fixture, kernel_run, scratch};

/// Where this suite'''s scratch volumes live, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "feature_matrix_oracle";

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::write::AttrChange;
use fs_xfs::{Error, Filesystem};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

struct MutationProbe {
    source: FileDevice,
    writes: AtomicUsize,
}

impl BlockRead for MutationProbe {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.source.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.source.size_bytes()
    }
}

impl BlockDevice for MutationProbe {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.source.write_at(offset, buf)
    }
    fn flush(&self) -> fs_core::Result<()> {
        self.source.flush()
    }
    fn is_writable(&self) -> bool {
        self.source.is_writable()
    }
}

#[test]
fn all_feature_fixtures_read_and_check_without_mutation() {
    let combos = selected("XFS_MATRIX_COMBOS", COMBOS);
    assert!(!combos.is_empty(), "no feature fixture selected");
    for combo in combos {
        let device = Arc::new(MutationProbe {
            source: FileDevice::open(fixture(&format!("xfsfeat-{combo}.img"))).unwrap(),
            writes: AtomicUsize::new(0),
        });
        let fs = Filesystem::mount(device.clone()).unwrap();
        let file = fs.lookup_path("/sf/data.bin").unwrap();
        assert_eq!(file.size, 32 * 4096, "{combo}: populated file");
        let report = fs_xfs::check::check(&fs);
        assert!(!report.dirty, "{combo}: dirty source fixture");
        assert!(report.inodes > 0 && report.directories > 0);
        assert!(report.is_clean(), "{combo}: {:?}", report.findings);
        assert_eq!(
            device.writes.load(Ordering::SeqCst),
            0,
            "{combo}: check wrote"
        );
    }
}

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
/// The rows, named as `build-feature-matrix-fixtures.sh` writes them.
const COMBOS: &[&str] = &[
    "v4",
    "base",
    "finobt",
    "finobt-inobtcount",
    "reflink",
    "reflink-finobt",
    "reflink-finobt-inobtcount",
    "rmapbt",
    "rmapbt-finobt",
    "rmapbt-finobt-inobtcount",
    "rmapbt-reflink-nofinobt",
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
    "meta_uuid",
    "quota",
    "stripe",
    "sector4k",
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
    "truncate_partly_shared",
    // The only operation here that allocates for a directory.
    "convert_directory",
    // An operation in an allocation group above the first.
    "create_in_later_group",
];

/// The rows and columns this run covers.
///
/// The whole matrix is twenty-eight images times eleven operations, and every
/// pair is a copy, a mount and a check inside the kernel. Iterating on
/// one failing pair should not cost the other two hundred, so
/// `XFS_MATRIX_COMBOS` and `XFS_MATRIX_OPS` take a comma-separated list
/// of names to keep. Unset means all of them, which is what CI runs.
///
/// A name that matches nothing is a typo, and a typo that quietly
/// selected an empty matrix would report a green run that checked
/// nothing.
fn selected(var: &str, all: &[&'static str]) -> Vec<&'static str> {
    let Ok(list) = std::env::var(var) else {
        return all.to_vec();
    };
    let wanted: Vec<&str> = list
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    for name in &wanted {
        assert!(
            all.contains(name),
            "{var} names {name:?}, which is not one of {all:?}"
        );
    }
    all.iter()
        .copied()
        .filter(|name| wanted.contains(name))
        .collect()
}

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
            if fs.superblock().features_ro_compat & fs_xfs::superblock::ro_compat::REFLINK == 0 {
                return Err("not applicable: no shared extent on this filesystem".into());
            }
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
        // Freeing a file that is part shared and part not. One extent,
        // three answers: the middle is this file's alone and goes back
        // to free space, and the two ends stay with the file that still
        // holds them.
        "truncate_partly_shared" => {
            let partial = match fs.lookup_path("/sf/partial.bin") {
                Ok(i) => i.ino,
                Err(_) => {
                    return Err("not applicable: no partly shared file on this filesystem".into())
                }
            };
            fs.truncate_to_zero(partial)
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
fn image_digest(img: &Path) -> Vec<u8> {
    let mut source = std::fs::File::open(img).unwrap();
    let mut digest = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    loop {
        let n = source.read(&mut buf).unwrap();
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    digest.finalize().to_vec()
}

fn exercise(img: &Path, combo: &str, op: &str) -> Outcome {
    let before = image_digest(img);
    let dev = Arc::new(MutationProbe {
        source: FileDevice::open_rw(img).expect("open read-write"),
        writes: AtomicUsize::new(0),
    });
    let fs = match Filesystem::mount_rw(dev.clone()) {
        Ok(fs) => fs,
        Err(Error::UnsupportedFeature(why)) => {
            assert_eq!(dev.writes.load(Ordering::SeqCst), 0, "mount refusal wrote");
            assert_eq!(
                image_digest(img),
                before,
                "mount refusal changed image bytes"
            );
            contract::require_expected(combo, op, Some(&why));
            return Outcome::Refused(why);
        }
        Err(e) => panic!("a read-write mount failed for a reason other than a refusal: {e}"),
    };

    let result = perform(&fs, op);
    drop(fs);

    // A refused operation wrote nothing, so there is nothing to judge
    // and nothing wrong: refusing is one of the two acceptable answers.
    if let Err(why) = result {
        assert_eq!(
            dev.writes.load(Ordering::SeqCst),
            0,
            "{op} refused after mutating the image: {why}"
        );
        assert_eq!(
            image_digest(img),
            before,
            "operation refusal changed image bytes"
        );
        contract::require_expected(combo, op, Some(&why));
        return Outcome::Refused(why);
    }
    contract::require_expected(combo, op, None);

    // The kernel replays what was logged, then the checker judges. Both
    // are the reference implementation; neither is this repository.
    let image = scratch::guest_path(img);
    let script = format!(
        r#"
        img=$(mktemp -u /tmp/feat-XXXXXX.img)
        export PATH=/usr/local/xfsprogs-parent/sbin:$PATH
        [ "$(xfs_repair -V 2>&1)" = "xfs_repair version 6.13.0" ] || exit 1
        cp {image} "$img"
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
                # RETRIED ONCE. A busy unmount under a loaded runner is
                # ordinary and clears in a moment; one that does not is the
                # failure worth reporting, because the kernel writes the
                # summary counters at unmount and nothing else does.
                if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
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
    // The replay and the checker always run: they run in the harness
    // guest, so every pair this reaches is judged.
    let repair = kernel_run(&script);
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

    let combos = selected("XFS_MATRIX_COMBOS", COMBOS);
    let ops = selected("XFS_MATRIX_OPS", OPS);

    for combo in &combos {
        // EVERY ROW IS BUILT, EVERY RUN. `chore fixtures -- feature-matrix`
        // makes one image per name in COMBOS with one pinned mkfs.xfs, so
        // a row whose image is not there is that build having gone wrong
        // -- and a row left out is a combination nobody checked, which
        // is the whole thing this matrix exists to prevent.
        let source = fixture(&format!("xfsfeat-{combo}.img"));

        for op in &ops {
            // A fresh copy per operation: the previous one may have
            // left a record in the log, and the next must start from
            // the filesystem as mkfs made it.
            let scratch = scratch::Volume::copy_of(
                SUITE,
                &source,
                &format!("xfsfeat-{combo}-{op}-scratch.img"),
            );
            checked += 1;

            match exercise(scratch.path(), combo, op) {
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

                    assert!(
                        !repair.contains("UMOUNT_FAILED"),
                        "{combo} / {op}: the volume could not be unmounted, so the \
                         summary counters were never written back to it. `xfs_repair` \
                         reports `sb_fdblocks N, counted N-1` for exactly that -- the \
                         free-block count it disagrees about is the one the unmount \
                         never wrote, not one this driver got wrong:\n{repair}"
                    );

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

    // THE FLOOR IS THE ONLY GUARD ON THE SELECTION. A missing image now
    // fails inside `fixture`, so zero pairs can only mean the selection
    // itself was empty: `XFS_MATRIX_COMBOS=` or `XFS_MATRIX_OPS=` naming
    // nothing keeps every name out and would otherwise report a green run
    // that exercised no combination at all.
    assert!(
        checked > 0,
        "no combination/operation pair was exercised. XFS_MATRIX_COMBOS and \
         XFS_MATRIX_OPS narrow the matrix and an empty one selects nothing; unset \
         them for the whole matrix, which is what CI runs. The images come from \
         `chore fixtures -- feature-matrix`."
    );
    eprintln!(
        "\n{checked} combination/operation pairs: {sound} written and sound, \
         {refused} refused by name, {not_applicable} not applicable, {unjudged} unjudged"
    );

    // Every selected pair needs a verdict; even one unjudged pair fails.
    require_every_pair_judged(unjudged, checked);

    assert!(
        broken.is_empty(),
        "these left a filesystem xfs_repair objects to. Each must either be maintained \
         properly or refused before the write:\n  {}",
        broken.join("\n  ")
    );
}

#[test]
fn declared_contract_covers_exactly_every_matrix_row_and_operation() {
    assert_eq!(OPS, contract::OPS);
    let rows: Vec<_> = contract::ROWS.iter().map(|(name, _)| *name).collect();
    assert_eq!(COMBOS, rows);
}

#[test]
fn pinned_feature_geometry_matches_independent_reference() {
    for combo in selected("XFS_MATRIX_COMBOS", COMBOS) {
        let image = fixture(&format!("xfsfeat-{combo}.img"));
        let evidence =
            std::fs::read_to_string(fixture(&format!("xfsfeat-{combo}.provenance"))).unwrap();
        for tool in ["mkfs.xfs", "xfs_info", "xfs_db", "xfs_quota", "xfs_repair"] {
            assert!(evidence
                .lines()
                .any(|line| line == format!("{tool} version 6.13.0")));
        }
        let hash: String = image_digest(&image)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert!(evidence
            .lines()
            .any(|line| line.starts_with(&format!("{hash}  "))));
        let reference =
            std::fs::read_to_string(fixture(&format!("xfsfeat-{combo}.sbdump"))).unwrap();
        let field = |name: &str| -> u64 {
            let value = reference
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name} = ")))
                .expect("missing independent field");
            // xfs_db appends decoded flag names after the numeric value.
            let value = value
                .split_whitespace()
                .next()
                .expect("empty independent field");
            if value == "null" {
                u64::MAX // xfs_db's NULLFSINO spelling
            } else if let Some(hex) = value.strip_prefix("0x") {
                u64::from_str_radix(hex, 16).unwrap()
            } else {
                value.parse().unwrap()
            }
        };
        let fs = Filesystem::mount(Arc::new(FileDevice::open(image).unwrap())).unwrap();
        let sb = fs.superblock();
        for (name, ours) in [
            ("versionnum", u64::from(sb.versionnum)),
            ("features2", u64::from(sb.features2)),
            ("bad_features2", u64::from(sb.bad_features2)),
            ("features_compat", u64::from(sb.features_compat)),
            ("features_ro_compat", u64::from(sb.features_ro_compat)),
            ("features_incompat", u64::from(sb.features_incompat)),
            ("features_log_incompat", u64::from(sb.features_log_incompat)),
            ("qflags", u64::from(sb.qflags)),
            ("blocksize", u64::from(sb.blocksize)),
            ("sectsize", u64::from(sb.sectsize)),
            ("inodesize", u64::from(sb.inodesize)),
            ("dirblklog", u64::from(sb.dirblklog)),
            ("unit", u64::from(sb.unit)),
            ("width", u64::from(sb.width)),
            ("rootino", sb.rootino),
            ("uquotino", sb.uquotino),
        ] {
            assert_eq!(ours, field(name), "{combo}: {name} disagrees with xfs_db");
        }
        match combo {
            "meta_uuid" => {
                assert_ne!(sb.uuid, sb.meta_uuid);
                assert_ne!(sb.features_incompat & 4, 0);
            }
            "quota" => {
                assert_ne!(sb.qflags, 0);
                assert!(sb.uquotino != 0 && sb.uquotino != u64::MAX);
            }
            "stripe" => {
                assert_eq!(field("unit"), 16);
                assert_eq!(field("width"), 64);
            }
            "sector4k" => {
                assert_eq!(sb.sectsize, 4096);
                assert_eq!(field("sectsize"), 4096);
            }
            _ => (),
        }
    }
}

/// Local return values only; this does not establish Linux correctness.
#[test]
fn selected_fixture_operations_match_declared_local_contracts() {
    let mut mismatches = Vec::new();
    for combo in selected("XFS_MATRIX_COMBOS", COMBOS) {
        let source = fixture(&format!("xfsfeat-{combo}.img"));
        for op in selected("XFS_MATRIX_OPS", OPS) {
            let scratch =
                scratch::Volume::copy_of(SUITE, &source, &format!("local-{combo}-{op}.img"));
            let dev = Arc::new(MutationProbe {
                source: FileDevice::open_rw(scratch.path()).unwrap(),
                writes: AtomicUsize::new(0),
            });
            let refusal = match Filesystem::mount_rw(dev.clone()) {
                Ok(fs) => perform(&fs, op).err(),
                Err(Error::UnsupportedFeature(why)) => Some(why),
                Err(e) => panic!("unexpected mount error: {e}"),
            };
            eprintln!("local {combo}/{op}: {refusal:?}");
            if std::panic::catch_unwind(|| {
                contract::require_expected(combo, op, refusal.as_deref())
            })
            .is_err()
            {
                mismatches.push(format!("{combo}/{op}: {refusal:?}"));
            }
            if refusal.is_some() {
                assert_eq!(dev.writes.load(Ordering::SeqCst), 0);
                assert_eq!(image_digest(scratch.path()), image_digest(&source));
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "local operation mismatches: {mismatches:?}"
    );
}

fn require_every_pair_judged(unjudged: usize, checked: usize) {
    assert_eq!(
        unjudged, 0,
        "{unjudged} of {checked} pairs went unjudged; every pair needs an oracle verdict"
    );
}

#[test]
#[should_panic(expected = "unjudged")]
fn even_one_unjudged_pair_fails_the_matrix() {
    require_every_pair_judged(1, 220);
}

/// The first line of a refusal, for a readable table.
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}
