//! `mkfs.xfs`: create a fresh v5 XFS filesystem.
//!
//! The options are the standard formatter's spelling for the few things
//! this one can choose: `-b size=`, `-d agcount=`, `-L`, `-m uuid=`, `-f`,
//! `-q` and `-N`. Anything else the standard formatter takes is REFUSED by
//! name rather than accepted and ignored: `-m reflink=0` or `-i size=256`
//! silently dropped would make a filesystem other than the one asked for,
//! and nobody would find out until something depended on the difference.
//!
//! The device or image must already exist at its size, as with the
//! standard formatter, unless `--size` is given for an image file that does
//! not exist yet. A device that already holds a filesystem is refused
//! without `-f`.
//!
//! A JSON report of what was written, read back from the new superblock,
//! goes to stdout; progress goes to stderr.

use std::sync::Arc;

use clap::{Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Json, Outcome, Tool};
use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::mkfs::{self, Options, MAX_LABEL_BYTES};

pub const TOOL: Tool = Tool {
    name: "mkfs.xfs",
    verb: "mkfs",
    section: 8,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "Create an XFS filesystem on a device or an image file",
    command,
    run,
};

fn command() -> Cmd {
    Cmd::new("mkfs.xfs")
        .about("Create an XFS filesystem on a device or an image file")
        .long_about(
            "Create a v5 XFS filesystem on a device or a pre-sized image file.\n\n\
             The device or file must already exist at the target size (`truncate -s 1G \
             disk.img`), unless --size is given for an image file that does not exist yet. \
             A device that already holds a filesystem is refused unless -f is given.\n\n\
             The filesystem has the standard formatter's default features: checksums, the \
             free inode btree, sparse inode chunks, reflink, large timestamps and directory \
             entry file types.\n\n\
             A JSON report of what was written, read back from the new superblock, goes to \
             stdout; progress goes to stderr.",
        )
        .arg(
            Arg::new("device")
                .value_name("TARGET")
                .help("Block device or image file to format")
                .required(true),
        )
        .arg(
            Arg::new("block")
                .short('b')
                .value_name("size=BYTES")
                .help(format!(
                    "Block size: a power of two, {}..={}. Default: {}.",
                    mkfs::MIN_BLOCK_SIZE,
                    mkfs::MAX_BLOCK_SIZE,
                    mkfs::DEFAULT_BLOCK_SIZE
                ))
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("data")
                .short('d')
                .value_name("agcount=N")
                .help("Number of allocation groups. Default: chosen from the size.")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("meta")
                .short('m')
                .value_name("uuid=UUID")
                .help("Volume UUID. Default: random.")
                .action(ArgAction::Append),
        )
        .arg(
            Arg::new("label")
                .short('L')
                .value_name("LABEL")
                .help(format!("Volume label, at most {MAX_LABEL_BYTES} bytes")),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .help("Format even if the device already holds a filesystem")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("dry-run")
                .short('N')
                .help("Work out the geometry and report it, but write nothing")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("quiet")
                .short('q')
                .help("No progress on stderr")
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("size")
                .long("size")
                .value_name("SIZE")
                .help(
                    "Create TARGET as an image file of SIZE bytes first, if it does not exist \
                     (K/M/G/T suffixes, 1024-based)",
                )
                .value_parser(parse_size),
        )
        .args(fs_core::cli::format_args())
        .after_help(
            "Examples:\n  \
             mkfs.xfs --size 1G -L BACKUP disk.img\n  \
             truncate -s 4G disk.img && mkfs.xfs -b size=4096 -d agcount=8 disk.img\n  \
             mkfs.xfs -f /dev/sdb1                  replace whatever is there\n  \
             mkfs.xfs -N disk.img                   report the geometry, write nothing",
        )
}

/// `-x key=value[,key=value]` over every occurrence of one option, each
/// key checked against what this formatter can honour.
fn suboptions(
    matches: &ArgMatches,
    id: &str,
    flag: char,
    known: &[&str],
) -> Result<Vec<(String, String)>, CliError> {
    let mut out = Vec::new();
    for value in matches.get_many::<String>(id).into_iter().flatten() {
        for pair in value.split(',') {
            let (key, val) = pair
                .split_once('=')
                .ok_or_else(|| CliError::usage(format!("-{flag} {pair}: expected key=value")))?;
            if !known.contains(&key) {
                return Err(CliError::usage(format!(
                    "-{flag} {key}= is not supported by this formatter (it can set: {}). \
                     Refused rather than ignored, so the filesystem made is the one asked for",
                    known.join(", ")
                )));
            }
            out.push((key.to_string(), val.to_string()));
        }
    }
    Ok(out)
}

fn options(matches: &ArgMatches) -> Result<Options, CliError> {
    let mut opts = Options::default();
    for (_, v) in suboptions(matches, "block", 'b', &["size"])? {
        opts.block_size = parse_bytes(&v)
            .and_then(|n| u32::try_from(n).ok())
            .ok_or_else(|| CliError::usage(format!("-b size={v}: not a size")))?;
    }
    for (_, v) in suboptions(matches, "data", 'd', &["agcount"])? {
        opts.agcount = Some(
            v.parse()
                .map_err(|_| CliError::usage(format!("-d agcount={v}: not a number")))?,
        );
    }
    for (_, v) in suboptions(matches, "meta", 'm', &["uuid"])? {
        opts.uuid = Some(
            parse_uuid(&v).ok_or_else(|| CliError::usage(format!("-m uuid={v}: not a UUID")))?,
        );
    }
    opts.label = matches.get_one::<String>("label").cloned();
    Ok(opts)
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let opts = options(matches)?;
    let quiet = matches.get_flag("quiet");
    let say = |line: String| {
        if !quiet {
            eprintln!("mkfs.xfs: {line}");
        }
    };
    let device = matches
        .get_one::<String>("device")
        .expect("clap requires the device")
        .as_str();
    let dry_run = matches.get_flag("dry-run");

    if let Some(n) = matches.get_one::<u64>("size").copied() {
        if std::fs::metadata(device).is_err() && !dry_run {
            let f = std::fs::File::create(device)
                .map_err(|e| CliError::failed(format!("--size: create {device}: {e}")))?;
            f.set_len(n)
                .map_err(|e| CliError::failed(format!("--size: set_len({n}) on {device}: {e}")))?;
            say(format!("--size: created {device} ({n} bytes)"));
        }
    }

    let dev = if dry_run {
        FileDevice::open(device)
    } else {
        FileDevice::open_rw(device)
    }
    .map_err(|e| CliError::failed(format!("open {device}: {e}")))?;
    let size = dev.size_bytes();

    // Work the geometry out before anything is inspected or written, so a
    // refusal names the real problem (a device too small, a bad option)
    // first.
    let plan = mkfs::plan(size, &opts).map_err(|e| CliError::failed(format!("{device}: {e}")))?;

    if !matches.get_flag("force") {
        if let Some(what) = mkfs::existing_signature(&dev) {
            return Err(CliError::refused(format!(
                "{device} already holds {what}. Use -f to replace it"
            )));
        }
    }

    let base = [
        ("fs", Json::from("xfs")),
        ("device", Json::from(device)),
        ("device_bytes", Json::from(size)),
        ("dry_run", Json::from(dry_run)),
    ];
    let mut report: Vec<(&str, Json)> = base.to_vec();
    if dry_run {
        report.push(("formatted", Json::from(false)));
        report.push(("block_size", Json::from(plan.block_size())));
        report.push(("ag_count", Json::from(plan.agcount())));
        report.push(("ag_blocks", Json::from(plan.agblocks())));
        report.push(("log_blocks", Json::from(plan.logblocks())));
        return Ok(Outcome::report(Json::object(report)).with_text(String::new()));
    }

    say(format!(
        "formatting {device} ({size} bytes, {} allocation groups of {} blocks of {} bytes)",
        plan.agcount(),
        plan.agblocks(),
        plan.block_size()
    ));
    mkfs::write(&dev, &plan).map_err(|e| CliError::failed(format!("{device}: {e}")))?;
    dev.flush()
        .map_err(|e| CliError::failed(format!("{device}: flush: {e}")))?;

    // The report is what the superblock now SAYS, read back through the
    // ordinary mount, not what was asked for.
    let dev: Arc<dyn BlockRead> = Arc::new(dev);
    let fs = fs_xfs::Filesystem::mount(dev)
        .map_err(|e| CliError::failed(format!("read back {device} after formatting: {e}")))?;
    let sb = fs.superblock();
    report.push(("formatted", Json::from(true)));
    report.push((
        "label",
        if sb.fname.is_empty() {
            Json::Null
        } else {
            Json::from(sb.fname.as_str())
        },
    ));
    report.push(("uuid", Json::from(format_uuid(&sb.uuid))));
    report.push(("block_size", Json::from(sb.blocksize)));
    report.push((
        "total_bytes",
        Json::from(sb.dblocks * u64::from(sb.blocksize)),
    ));
    report.push(("ag_count", Json::from(sb.agcount)));
    report.push(("ag_blocks", Json::from(sb.agblocks)));
    report.push(("log_blocks", Json::from(sb.logblocks)));
    report.push(("free_blocks", Json::from(sb.fdblocks)));
    say(format!("{device} formatted"));
    Ok(Outcome::report(Json::object(report)).with_text(String::new()))
}

/// The UUID in its standard 8-4-4-4-12 form.
fn format_uuid(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 32 hex digits, dashes anywhere.
fn parse_uuid(v: &str) -> Option<[u8; 16]> {
    let hex: String = v.chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// A byte count with an optional 1024-based K/M/G/T suffix.
fn parse_bytes(v: &str) -> Option<u64> {
    let (digits, shift) = match v.chars().last()?.to_ascii_uppercase() {
        'K' => (&v[..v.len() - 1], 10),
        'M' => (&v[..v.len() - 1], 20),
        'G' => (&v[..v.len() - 1], 30),
        'T' => (&v[..v.len() - 1], 40),
        _ => (v, 0),
    };
    digits.parse::<u64>().ok()?.checked_mul(1u64 << shift)
}

fn parse_size(v: &str) -> Result<u64, String> {
    parse_bytes(v)
        .filter(|n| *n > 0)
        .ok_or_else(|| format!("not a size: {v}"))
}
