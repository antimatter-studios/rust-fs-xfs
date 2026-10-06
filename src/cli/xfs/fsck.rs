//! `fsck.xfs`: check an XFS filesystem without changing it.
//!
//! What it checks is this crate's checker (`fs_xfs::check`): the
//! superblock copies, every allocation group's headers against its
//! btrees, one owner for every block, the inode btrees against the inodes,
//! the directory tree and link counts, and the superblock's counters. It
//! is a subset of what `xfs_repair -n` checks, and says so: a volume this
//! calls clean is one on which none of THESE invariants is broken.
//!
//! It never writes. `-n` is accepted because scripts pass it; `-y` and
//! `-p` (repair) are refused, because there is no repair.
//!
//! EXIT STATUS IS fsck(8)'s, because scripts and the `fsck` front-end read
//! it: 0 clean, 4 errors left uncorrected, 8 an operational error (the
//! target could not be opened, or is not XFS), 16 a wrong command line.

use std::ffi::OsString;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_core::{BlockRead, FileDevice, OwnedSlice};

/// fsck(8): no errors.
pub const CLEAN: u8 = 0;
/// fsck(8): filesystem errors left uncorrected.
pub const UNCORRECTED: u8 = 4;
/// fsck(8): operational error.
pub const OPERATIONAL: u8 = 8;
/// fsck(8): usage or syntax error.
pub const USAGE: u8 = 16;

pub const TOOL: Tool = Tool {
    name: "fsck.xfs",
    verb: "fsck",
    section: 8,
    usage_exit: USAGE,
    about: "Check an XFS filesystem without changing it",
    command,
    run,
};

fn command() -> Cmd {
    Cmd::new("fsck.xfs")
        .about("Check an XFS filesystem without changing it")
        .long_about(
            "Check an XFS image or device and report what is wrong with it. Nothing is \
             written, and nothing is repaired.\n\n\
             Checked: the superblock copies against the primary; every allocation group's \
             headers against its free-space, inode and refcount btrees; that every block has \
             exactly one owner (a block claimed twice is a cross-link, a block claimed by \
             nothing is lost); every inode the inode btree calls allocated or free against the \
             inode itself; the directory tree from the root, with every entry pointing at an \
             allocated inode of the type it records and every link count matching the entries \
             that reach the inode; and the superblock's counters. A volume whose log needed \
             replay is checked as the replay leaves it, and reported as dirty.\n\n\
             Exit status is fsck(8)'s: 0 clean, 4 errors found (and left), 8 the target could \
             not be checked, 16 a wrong command line.",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("Block device or image file to check")
                .required(true)
                .value_parser(value_parser!(OsString)),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help(
                    "Where the filesystem starts inside TARGET (a partition in a whole-disk image)",
                )
                .default_value("0")
                .value_parser(value_parser!(u64)),
        )
        .arg(
            Arg::new("no-change")
                .short('n')
                .help("Check only (the default, and the only mode)")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("repair")
                .short('y')
                .help("Refused: this checker does not repair")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("preen")
                .short('p')
                .help("Refused: this checker does not repair")
                .action(ArgAction::SetTrue),
        )
        .args(fs_core::cli::format_args())
        .after_help(
            "Examples:\n  \
             fsck.xfs disk.img                     the report, as JSON\n  \
             fsck.xfs --text disk.img              the findings, one per line\n  \
             fsck.xfs --offset 1048576 whole.img   a partition inside a disk image",
        )
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    if matches.get_flag("repair") || matches.get_flag("preen") {
        return Err(CliError::usage(
            "fsck.xfs checks and does not repair: -y and -p are refused rather than \
             ignored, so a script that asked for a repair is not told one happened",
        )
        .with_code(USAGE));
    }
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let name = target.to_string_lossy().into_owned();
    let offset = *matches.get_one::<u64>("offset").expect("defaulted");

    let dev: Arc<dyn BlockRead> = Arc::new(
        FileDevice::open(&*name)
            .map_err(|e| CliError::failed(format!("open {name}: {e}")).with_code(OPERATIONAL))?,
    );
    let dev: Arc<dyn BlockRead> = if offset == 0 {
        dev
    } else {
        let size = dev.size_bytes();
        if offset >= size {
            return Err(CliError::failed(format!(
                "--offset {offset} is past the end of {name} ({size} bytes)"
            ))
            .with_code(OPERATIONAL));
        }
        Arc::new(OwnedSlice::new(dev, offset, size - offset))
    };

    let base = |clean: bool, dirty: bool, code: u8| -> Vec<(&'static str, Json)> {
        vec![
            ("fs", Json::from("xfs")),
            ("device", Json::from(name.as_str())),
            ("clean", Json::from(clean)),
            ("dirty", Json::from(dirty)),
            ("exit", Json::from(u64::from(code))),
        ]
    };

    let fs = match fs_xfs::Filesystem::mount(dev) {
        Ok(fs) => fs,
        // Not XFS at all, or a device that cannot be read: there is
        // nothing to check.
        Err(e @ (fs_xfs::Error::NotXfs { .. } | fs_xfs::Error::Io(_))) => {
            return Err(CliError::failed(format!("{name}: {e}")).with_code(OPERATIONAL))
        }
        // XFS, and too damaged to mount: that is a finding.
        Err(e) => {
            let what = format!("the filesystem cannot be mounted: {e}");
            let mut report = base(false, false, UNCORRECTED);
            report.push((
                "findings",
                Json::Arr(vec![Json::object([("what", Json::from(what.as_str()))])]),
            ));
            return Ok(Outcome::report(Json::object(report))
                .with_text(format!("{name}: {what}"))
                .with_code(UNCORRECTED));
        }
    };

    let checked = fs_xfs::check::check(&fs);
    let code = if checked.is_clean() {
        CLEAN
    } else {
        UNCORRECTED
    };
    let mut report = base(checked.is_clean(), checked.dirty, code);
    report.push(("inodes", Json::from(checked.inodes)));
    report.push(("directories", Json::from(checked.directories)));
    report.push(("free_blocks", Json::from(checked.free_blocks)));
    report.push((
        "findings",
        Json::Arr(
            checked
                .findings
                .iter()
                .map(|f| {
                    Json::object([
                        (
                            "ag",
                            f.ag.map(|a| Json::from(u64::from(a))).unwrap_or(Json::Null),
                        ),
                        ("ino", f.ino.map(Json::from).unwrap_or(Json::Null)),
                        ("what", Json::from(f.what.as_str())),
                    ])
                })
                .collect(),
        ),
    ));
    let mut text: Vec<String> = checked
        .findings
        .iter()
        .map(|f| format!("{name}: {}", f.what))
        .collect();
    if checked.dirty {
        text.insert(
            0,
            format!("{name}: the log held unapplied records; checked as replayed"),
        );
    }
    if checked.is_clean() {
        text.push(format!(
            "{name}: clean, {} inodes, {} directories",
            checked.inodes, checked.directories
        ));
    }
    Ok(Outcome::report(Json::object(report))
        .with_text(text.join("\n"))
        .with_code(code))
}
