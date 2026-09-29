//! `fs.xfs <target> <verb>`: an errand inside an XFS image or device,
//! without mounting it.
//!
//! The verbs are the shared set. Metadata is JSON (or `--text`). A verb
//! the library cannot do yet still exists and answers `not implemented`
//! with exit status 3, so a script moved between filesystems fails loudly
//! instead of meaning something else.

use std::ffi::OsString;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Json, Outcome, Tool};
use fs_xfs::Filesystem;

pub const TOOL: Tool = Tool {
    name: "fs.xfs",
    verb: "fs",
    section: 1,
    usage_exit: crate::common::output::EXIT_USAGE,
    about: "Inspect an XFS image or device without mounting it",
    command,
    run,
};

/// The canonical keys every `fs.<fs>` answers, in the shared order.
/// Filesystem specifics are nested under `xfs`.
pub const KEYS: &[&str] = &[
    "fs",
    "label",
    "total_bytes",
    "free_bytes",
    "block_size",
    "dirty",
    "xfs",
];

fn command() -> Cmd {
    Cmd::new("fs.xfs")
        .about("Inspect an XFS image or device without mounting it")
        .long_about(
            "Work inside an XFS image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (read the label, check whether the log needs \
             replaying), not a place to do real filesystem work: for that, mount it.\n\n\
             Metadata is JSON on stdout (--text for people). A failure is \
             {\"error\": \"...\", \"code\": N} on stderr, N being the exit status: \
             1 failed, 2 wrong command line, 3 not implemented.",
        )
        .arg(
            Arg::new("target")
                .value_name("TARGET")
                .help("The image file or device")
                .value_parser(value_parser!(OsString))
                .required(true),
        )
        .arg(
            Arg::new("offset")
                .long("offset")
                .value_name("BYTES")
                .help(
                    "Where the filesystem starts in TARGET, for a partition in a whole-disk image",
                )
                .value_parser(value_parser!(u64))
                .global(true),
        )
        .args(crate::common::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(key_command(
            "get",
            "Report the filesystem's properties, or one of them",
        ))
        .subcommand(key_command(
            "info",
            "The same as get: every property, or one of them",
        ))
        .subcommand(
            Cmd::new("set")
                .about("Change a property (label: not implemented yet)")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.xfs disk.img set label BACKUP\n\n\
                     Answers `not implemented` (exit 3): the library has no writer for \
                     the XFS label.",
                ),
        )
        .subcommand(
            Cmd::new("resize")
                .about("Grow or shrink the filesystem (not implemented)")
                .arg(Arg::new("size").value_name("SIZE").required(true))
                .arg(
                    Arg::new("force")
                        .long("force")
                        .action(ArgAction::SetTrue)
                        .help("Do it without asking"),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img resize 20G --force\n\n\
                     Answers `not implemented` (exit 3): the library has no resize.",
                ),
        )
        .after_help(
            "Examples:\n  fs.xfs disk.img get\n  \
             fs.xfs disk.img get label --text\n  \
             fs.xfs --offset 1048576 whole-disk.img info",
        )
}

fn key_command(name: &'static str, about: &'static str) -> Cmd {
    Cmd::new(name)
        .about(about)
        .arg(
            Arg::new("key")
                .value_name("KEY")
                .help(format!("One of: {} (or xfs.<field>)", KEYS.join(", "))),
        )
        .after_help(format!(
            "Examples:\n  fs.xfs disk.img {name}\n  \
             fs.xfs disk.img {name} label --text\n  \
             fs.xfs disk.img {name} xfs.uuid"
        ))
}

fn run(matches: &ArgMatches) -> Result<Outcome, CliError> {
    let target = matches
        .get_one::<OsString>("target")
        .expect("clap requires the target");
    let (verb, sub) = matches.subcommand().expect("clap requires a verb");
    let offset = sub
        .get_one::<u64>("offset")
        .or_else(|| matches.get_one::<u64>("offset"))
        .copied()
        .unwrap_or(0);
    match verb {
        "get" | "info" => get(
            &super::device::mount(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        "set" => set(sub),
        "resize" => Err(CliError::not_implemented(
            "resize: this library cannot resize an XFS filesystem",
        )),
        other => unreachable!("clap knows no verb {other}"),
    }
}

/// The UUID in its standard 8-4-4-4-12 form.
fn uuid_text(u: &[u8; 16]) -> String {
    let hex: String = u.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// The envelope: the shared keys first, XFS's own under `xfs`.
///
/// `dirty` is true when the volume's log held records nothing had applied,
/// which this mount replayed into memory to read through: what the rest of
/// the report describes is then the filesystem the kernel would recover,
/// not the structures on disk.
pub fn envelope(fs: &Filesystem) -> Json {
    let sb = fs.superblock();
    let block_size = u64::from(sb.blocksize);
    Json::object([
        ("fs", Json::from("xfs")),
        (
            "label",
            if sb.fname.is_empty() {
                Json::Null
            } else {
                Json::from(sb.fname.as_str())
            },
        ),
        ("total_bytes", Json::from(sb.dblocks * block_size)),
        ("free_bytes", Json::from(sb.fdblocks * block_size)),
        ("block_size", Json::from(block_size)),
        ("dirty", Json::from(fs.was_replayed())),
        (
            "xfs",
            Json::object([
                ("version", Json::from(sb.version())),
                ("uuid", Json::from(uuid_text(&sb.uuid))),
                ("ag_count", Json::from(sb.agcount)),
                ("ag_blocks", Json::from(sb.agblocks)),
                ("total_blocks", Json::from(sb.dblocks)),
                ("free_blocks", Json::from(sb.fdblocks)),
                ("inode_count", Json::from(sb.icount)),
                ("free_inodes", Json::from(sb.ifree)),
                ("sector_size", Json::from(sb.sectsize)),
                ("inode_size", Json::from(sb.inodesize)),
                ("root_inode", Json::from(sb.rootino)),
                ("log_blocks", Json::from(sb.logblocks)),
                ("versionnum", Json::from(sb.versionnum)),
                ("features2", Json::from(sb.features2)),
                ("features_compat", Json::from(sb.features_compat)),
                ("features_ro_compat", Json::from(sb.features_ro_compat)),
                ("features_incompat", Json::from(sb.features_incompat)),
                (
                    "features_log_incompat",
                    Json::from(sb.features_log_incompat),
                ),
            ]),
        ),
    ])
}

fn get(fs: &Filesystem, key: Option<&str>) -> Result<Outcome, CliError> {
    let all = envelope(fs);
    let Some(key) = key else {
        return Ok(Outcome::report(all));
    };
    let mut value = Some(&all);
    for part in key.split('.') {
        value = value.and_then(|v| v.get(part));
    }
    let Some(value) = value else {
        return Err(CliError::usage(format!(
            "no key {key:?}; the keys are {} (and xfs.<field>)",
            KEYS.join(", ")
        )));
    };
    let text = value.to_text();
    Ok(Outcome::report(Json::object([(key, value.clone())])).with_text(text))
}

fn set(sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => Err(CliError::not_implemented(
            "set label: this library has no writer for the XFS volume label",
        )),
        k if KEYS.contains(&k) || k.starts_with("xfs.") => {
            Err(CliError::refused(format!("{k} is read-only")))
        }
        other => Err(CliError::usage(format!(
            "no key {other:?}; the settable key is label"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_is_written_in_its_standard_groups() {
        let u = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ];
        assert_eq!(uuid_text(&u), "01234567-89ab-cdef-0011-223344556677");
    }
}
