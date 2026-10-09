//! `fsck.xfs`: check an XFS filesystem without changing it.
//!
//! What it checks is this crate's checker (`fs_xfs::check`): the
//! superblock copies, every allocation group's headers against its
//! btrees, one owner for every block, the inode btrees against the inodes,
//! the directory tree and link counts, and the superblock's counters. It
//! is a subset of what `xfs_repair -n` checks, and says so: a volume this
//! calls clean is one on which none of THESE invariants is broken.
//!
//! By default it never writes, and `-n` says so for scripts that pass it.
//! `--dry-run` plans a repair (#375): it takes the target for itself,
//! then prints what a repair would change and what it would leave, and
//! still writes nothing. `-y` and `-p` make the plan and apply it (#391):
//! only what a rule owns is changed, a refused plan changes nothing, and
//! the volume is checked again afterwards from a fresh mount.
//!
//! THE JSON REPORT IS VERSIONED (#363). Its shape is documented in
//! `docs/fsck-output.md`, and [`SCHEMA_VERSION`] changes only when a key
//! is removed or its meaning changes; a key may be added without one.
//! Every finding carries a stable `code` and a `severity`; the `what`
//! beside them is for a person, and may be reworded in any release.
//!
//! EXIT STATUS IS fsck(8)'s, because scripts and the `fsck` front-end read
//! it: 0 clean, 1 errors corrected, 4 errors left uncorrected (1 and 4 can
//! both be set), 8 an operational error (the target could not be opened,
//! or is not XFS), 16 a wrong command line.

use std::ffi::OsString;
use std::sync::Arc;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_core::{BlockRead, FileDevice, OwnedSlice};

/// fsck(8): no errors.
pub const CLEAN: u8 = 0;
/// fsck(8): filesystem errors corrected.
pub const CORRECTED: u8 = 1;
/// fsck(8): filesystem errors left uncorrected.
pub const UNCORRECTED: u8 = 4;
/// fsck(8): operational error.
pub const OPERATIONAL: u8 = 8;
/// fsck(8): usage or syntax error.
pub const USAGE: u8 = 16;

/// What the report's `schema` key says it is.
pub const SCHEMA: &str = "rust-fs-xfs/fsck";
/// The report's `schema_version`.
pub const SCHEMA_VERSION: u64 = 1;

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
             JSON reports use schema rust-fs-xfs/fsck version 1. Findings carry stable codes, \
             severity and location. scan is complete, partial, or none; a partial scan is \
             never clean. The schema and codes are documented in docs/fsck-output.md.\n\n\
             --dry-run plans a repair and prints the plan under the report's plan key. It \
             takes the target for itself first (an exclusive lock, and on Linux no mount or \
             loop device using it), refuses volumes it cannot reason about with repair.* \
             findings, and writes nothing.\n\n\
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
                .help("Check only, which is the default")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dry-run")
                .long("dry-run")
                .help("Plan a repair and print it, writing nothing")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("repair")
                .short('y')
                .help("Repair what a rule owns, after planning it; a refused plan writes nothing")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("preen")
                .short('p')
                .help("The same as -y: every repair this makes is one it can make unattended")
                .action(ArgAction::SetTrue),
        )
        .args(fs_core::cli::format_args())
        .after_help(
            "Examples:\n  \
             fsck.xfs disk.img                     the report, as JSON\n  \
             fsck.xfs --text disk.img              the findings, one per line\n  \
             fsck.xfs --dry-run disk.img           what a repair would change\n  \
             fsck.xfs --offset 1048576 whole.img   a partition inside a disk image",
        )
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let repairing = matches.get_flag("repair") || matches.get_flag("preen");
    if repairing && (matches.get_flag("no-change") || matches.get_flag("dry-run")) {
        return Err(CliError::usage(
            "-y and -p repair, and -n and --dry-run promise not to: asked for both, \
             fsck.xfs does neither",
        )
        .with_code(USAGE));
    }
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let name = target.to_string_lossy().into_owned();
    let offset = *matches.get_one::<u64>("offset").expect("defaulted");
    let dry_run = matches.get_flag("dry-run");

    // Taken before anything is read, and held until the plan is made.
    let access = if dry_run || repairing {
        match fs_xfs::repair::Exclusive::claim(std::path::Path::new(target)) {
            Ok(access) => Some(access),
            Err(refusal) => {
                let text = format!(
                    "{name}: plan: refused: {}: {}",
                    refusal.code.as_str(),
                    refusal.what
                );
                let report = Json::object([
                    ("schema", Json::from(SCHEMA)),
                    ("schema_version", Json::from(SCHEMA_VERSION)),
                    ("fs", Json::from("xfs")),
                    ("device", Json::from(name.as_str())),
                    ("clean", Json::from(false)),
                    ("dirty", Json::from(false)),
                    ("scan", Json::from("none")),
                    ("exit", Json::from(u64::from(OPERATIONAL))),
                    ("findings", Json::Arr(Vec::new())),
                    ("suppressed", Json::from(0u64)),
                    ("plan", refused_plan(&refusal)),
                ]);
                return Ok(Outcome::report(report)
                    .with_text(text)
                    .with_code(OPERATIONAL));
            }
        }
    } else {
        None
    };

    // Read-write only when a repair was asked for; the mount reads
    // through it either way.
    let writable = if repairing {
        Some(Arc::new(FileDevice::open_rw(&*name).map_err(|e| {
            CliError::failed(format!("open {name} to repair it: {e}")).with_code(OPERATIONAL)
        })?))
    } else {
        None
    };
    let dev: Arc<dyn BlockRead> =
        match &writable {
            Some(rw) => rw.clone(),
            None => Arc::new(FileDevice::open(&*name).map_err(|e| {
                CliError::failed(format!("open {name}: {e}")).with_code(OPERATIONAL)
            })?),
        };
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

    let base = |clean: bool, dirty: bool, scan: &str, code: u8| -> Vec<(&'static str, Json)> {
        vec![
            ("schema", Json::from(SCHEMA)),
            ("schema_version", Json::from(SCHEMA_VERSION)),
            ("fs", Json::from("xfs")),
            ("device", Json::from(name.as_str())),
            ("clean", Json::from(clean)),
            ("dirty", Json::from(dirty)),
            ("scan", Json::from(scan)),
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
        // XFS, and too damaged to mount: that is a finding, and nothing
        // under it was checked.
        Err(e) => {
            let what = format!("the filesystem cannot be mounted: {e}");
            let mut report = base(false, false, "none", UNCORRECTED);
            report.push((
                "findings",
                Json::Arr(vec![finding(&fs_xfs::check::Finding {
                    code: fs_xfs::check::Code::Mount,
                    location: fs_xfs::check::Location::default(),
                    what: what.clone(),
                })]),
            ));
            report.push(("suppressed", Json::from(0u64)));
            if dry_run {
                report.push((
                    "plan",
                    refused_plan(&fs_xfs::check::Finding {
                        code: fs_xfs::check::Code::RepairIncomplete,
                        location: fs_xfs::check::Location::default(),
                        what: "the filesystem could not be mounted, so nothing was checked".into(),
                    }),
                ));
            }
            return Ok(Outcome::report(Json::object(report))
                .with_text(format!("{name}: mount: {what}"))
                .with_code(UNCORRECTED));
        }
    };

    let planned = access
        .as_ref()
        .map(|access| fs_xfs::repair::plan(&fs, access));
    // A repair applies the plan, then checks again from a fresh mount: the
    // report is the volume as the repair left it.
    let mut applied = None;
    let checked = match (&planned, &writable, &access) {
        (Some(plan), Some(rw), Some(access)) => {
            let ready = plan.status == fs_xfs::repair::Status::Ready;
            let n = if ready && !plan.changes.is_empty() {
                fs_xfs::repair::apply(plan, rw.as_ref(), offset, access).map_err(|e| {
                    CliError::failed(format!("{name}: repair: {e}")).with_code(OPERATIONAL)
                })?
            } else {
                0
            };
            applied = Some(n);
            if n > 0 {
                drop(fs);
                recheck(&name, offset)?
            } else {
                plan.check.clone()
            }
        }
        (Some(plan), _, _) => plan.check.clone(),
        _ => fs_xfs::check::check(&fs),
    };
    let corrected = if applied.unwrap_or(0) > 0 {
        CORRECTED
    } else {
        CLEAN
    };
    // A repair asked for and refused leaves whatever the plan would have
    // dealt with: it is never reported as clean, even when the check alone
    // found nothing but warnings (a log that needed replay, for one).
    let refused = applied.is_some()
        && planned
            .as_ref()
            .is_some_and(|p| p.status == fs_xfs::repair::Status::Refused);
    let code = corrected
        | if checked.is_clean() && !refused {
            CLEAN
        } else {
            UNCORRECTED
        };
    let mut report = base(
        checked.is_clean(),
        checked.dirty,
        checked.scan.as_str(),
        code,
    );
    report.push(("inodes", Json::from(checked.inodes)));
    report.push(("directories", Json::from(checked.directories)));
    report.push(("free_blocks", Json::from(checked.free_blocks)));
    report.push((
        "findings",
        Json::Arr(checked.findings.iter().map(finding).collect()),
    ));
    report.push(("suppressed", Json::from(checked.suppressed)));
    let mut text: Vec<String> = checked
        .findings
        .iter()
        .map(|f| format!("{name}: {}: {}", f.code.as_str(), f.what))
        .collect();
    if checked.suppressed > 0 {
        text.push(format!(
            "{name}: {} more findings are not listed",
            checked.suppressed
        ));
    }
    if checked.scan == fs_xfs::check::Scan::Partial {
        text.push(format!(
            "{name}: the scan is partial: what could not be read was not checked"
        ));
    }
    if checked.is_clean() {
        text.push(format!(
            "{name}: clean, {} inodes, {} directories",
            checked.inodes, checked.directories
        ));
    }
    if let Some(plan) = &planned {
        report.push(("plan", plan_json(plan)));
        text.extend(plan_text(&name, plan));
    }
    if let Some(n) = applied {
        report.push(("applied", Json::from(n as u64)));
        text.push(format!("{name}: repair: {n} changes written"));
    }
    Ok(Outcome::report(Json::object(report))
        .with_text(text.join("\n"))
        .with_code(code))
}

/// A plan refused for `refusal`, with nothing proposed.
fn refused_plan(refusal: &fs_xfs::check::Finding) -> Json {
    Json::object([
        (
            "status",
            Json::from(fs_xfs::repair::Status::Refused.as_str()),
        ),
        ("changes", Json::Arr(Vec::new())),
        ("refusals", Json::Arr(vec![finding(refusal)])),
        ("unplanned", Json::Arr(Vec::new())),
    ])
}

/// A plan, as the report's `plan` key holds it.
fn plan_json(plan: &fs_xfs::repair::Plan) -> Json {
    let changes = plan
        .changes
        .iter()
        .map(|c| {
            Json::object([
                ("offset", Json::from(c.offset)),
                ("length", Json::from(c.after.len() as u64)),
                ("code", Json::from(c.code.as_str())),
                ("rule", Json::from(c.rule)),
                (
                    "before_crc32c",
                    Json::from(u64::from(crc32c::crc32c(&c.before))),
                ),
                (
                    "after_crc32c",
                    Json::from(u64::from(crc32c::crc32c(&c.after))),
                ),
                ("what", Json::from(c.what.as_str())),
            ])
        })
        .collect();
    Json::object([
        ("status", Json::from(plan.status.as_str())),
        ("changes", Json::Arr(changes)),
        (
            "refusals",
            Json::Arr(plan.refusals.iter().map(finding).collect()),
        ),
        (
            "unplanned",
            Json::Arr(plan.unplanned.iter().map(finding).collect()),
        ),
    ])
}

/// A plan, one line per refusal or change, and a summary.
fn plan_text(name: &str, plan: &fs_xfs::repair::Plan) -> Vec<String> {
    let mut lines: Vec<String> = plan
        .refusals
        .iter()
        .map(|f| format!("{name}: plan: refused: {}: {}", f.code.as_str(), f.what))
        .collect();
    lines.extend(plan.changes.iter().map(|c| {
        format!(
            "{name}: plan: {} bytes at {}: {} ({}): {}",
            c.after.len(),
            c.offset,
            c.code.as_str(),
            c.rule,
            c.what
        )
    }));
    if plan.status == fs_xfs::repair::Status::Ready {
        lines.push(format!(
            "{name}: plan: ready, {} changes, {} findings no rule repairs, nothing written",
            plan.changes.len(),
            plan.unplanned.len()
        ));
    }
    lines
}

/// One finding, as the report lists it.
fn finding(f: &fs_xfs::check::Finding) -> Json {
    let at = &f.location;
    Json::object([
        ("code", Json::from(f.code.as_str())),
        ("severity", Json::from(f.severity().as_str())),
        (
            "ag",
            at.ag
                .map(|a| Json::from(u64::from(a)))
                .unwrap_or(Json::Null),
        ),
        (
            "agbno",
            at.agbno
                .map(|b| Json::from(u64::from(b)))
                .unwrap_or(Json::Null),
        ),
        ("ino", at.ino.map(Json::from).unwrap_or(Json::Null)),
        ("field", at.field.map(Json::from).unwrap_or(Json::Null)),
        ("what", Json::from(f.what.as_str())),
    ])
}

/// The volume checked again, through a mount of its own, after a repair
/// wrote to it.
fn recheck(name: &str, offset: u64) -> Result<fs_xfs::check::Report, CliError> {
    let failed = |e: String| {
        CliError::failed(format!("{name}: after the repair: {e}")).with_code(OPERATIONAL)
    };
    let dev: Arc<dyn BlockRead> =
        Arc::new(FileDevice::open(name).map_err(|e| failed(e.to_string()))?);
    let dev: Arc<dyn BlockRead> = if offset == 0 {
        dev
    } else {
        let size = dev.size_bytes();
        Arc::new(OwnedSlice::new(dev, offset, size - offset))
    };
    let fs = fs_xfs::Filesystem::mount(dev).map_err(|e| failed(e.to_string()))?;
    Ok(fs_xfs::check::check(&fs))
}
