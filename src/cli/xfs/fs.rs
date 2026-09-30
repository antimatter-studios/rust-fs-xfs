//! `fs.xfs <target> <verb>`: an errand inside an XFS image or device,
//! without mounting it.
//!
//! The verbs are the shared set. Metadata is JSON (or `--text`); file
//! content is raw bytes. A verb the library cannot do yet still exists and
//! answers `not implemented` with exit status 3, so a script moved between
//! filesystems fails loudly instead of meaning something else.

use std::ffi::OsString;
use std::io::Write;

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use crate::common::{CliError, Json, Outcome, Tool};
use fs_xfs::inode::{FileType, Inode};
use fs_xfs::Filesystem;

pub const TOOL: Tool = Tool {
    name: "fs.xfs",
    verb: "fs",
    section: 1,
    usage_exit: crate::common::output::EXIT_USAGE,
    about: "List, read and inspect an XFS image or device without mounting it",
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
        .about("List, read and inspect an XFS image or device without mounting it")
        .long_about(
            "Work inside an XFS image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, read the label, check whether \
             the log needs replaying), not a place to do real filesystem work: for that, \
             mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` writes the file's raw \
             bytes. A failure is {\"error\": \"...\", \"code\": N} on stderr, N being \
             the exit status: 1 failed, 2 wrong command line, 3 not implemented.",
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
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img ls /etc\n  \
                     fs.xfs disk.img ls / | jq -r '.[].name'\n  \
                     fs.xfs disk.img ls --text /",
                ),
        )
        .subcommand(
            Cmd::new("read")
                .about("Write a file's bytes to stdout, or to a file with -o")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .value_parser(value_parser!(OsString))
                        .help("Write here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img read /etc/hostname\n  \
                     fs.xfs disk.img read /var/log/syslog | grep -i error\n  \
                     fs.xfs disk.img read /backup.tar -o backup.tar",
                ),
        )
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
            "Examples:\n  fs.xfs disk.img ls /\n  \
             fs.xfs disk.img read /etc/fstab > fstab\n  \
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
        "ls" => ls(&super::device::mount(target, offset)?, path_arg(sub)),
        "read" => read(
            &super::device::mount(target, offset)?,
            path_arg(sub),
            sub.get_one("output"),
        ),
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

fn path_arg(sub: &ArgMatches) -> &[u8] {
    let path = sub
        .get_one::<OsString>("path")
        .expect("clap requires or defaults the path");
    os_bytes(path)
}

/// A path as the bytes the filesystem compares, not as text: XFS names are
/// bytes and need not be UTF-8, and a name `ls` printed must resolve when
/// it is handed back.
#[cfg(unix)]
fn os_bytes(s: &OsString) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;
    s.as_bytes()
}

#[cfg(not(unix))]
fn os_bytes(s: &OsString) -> &[u8] {
    s.to_str().map(str::as_bytes).unwrap_or_default()
}

fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn xfs_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    CliError::failed(format!("{}: {e}", show(what)))
}

fn type_name(t: Option<FileType>) -> &'static str {
    match t {
        Some(FileType::Regular) => "file",
        Some(FileType::Directory) => "dir",
        Some(FileType::Symlink) => "symlink",
        Some(FileType::CharDevice) => "char",
        Some(FileType::BlockDevice) => "block",
        Some(FileType::Fifo) => "fifo",
        Some(FileType::Socket) => "socket",
        None => "unknown",
    }
}

fn type_char(name: &str) -> char {
    match name {
        "file" => '-',
        "dir" => 'd',
        "symlink" => 'l',
        "char" => 'c',
        "block" => 'b',
        "fifo" => 'p',
        "socket" => 's',
        _ => '?',
    }
}

/// One `ls` entry: the fields every `fs.<fs>` reports, typed the same way
/// everywhere -- name (string), type (string), size (number), mode (octal
/// string), mtime (seconds since the epoch, number), inode (number), and
/// target (string) for a symlink. A name that is not UTF-8 is shown
/// lossily, with its exact bytes in `name_hex`.
fn entry(name: &[u8], inode: &Inode, target: Option<Vec<u8>>) -> Json {
    let kind = type_name(inode.file_type());
    let mut fields = vec![("name", Json::from(show(name)))];
    if std::str::from_utf8(name).is_err() {
        fields.push((
            "name_hex",
            Json::from(name.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("type", Json::from(kind)),
        ("size", Json::from(inode.size)),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("mtime", Json::from(inode.mtime.sec)),
        ("inode", Json::from(inode.ino)),
    ]);
    if let Some(target) = target {
        fields.push(("target", Json::from(show(&target))));
    }
    Json::object(fields)
}

fn entry_text(e: &Json) -> String {
    let field = |k: &str| e.get(k).map(Json::to_text).unwrap_or_default();
    let mut line = format!(
        "{}{} {:>12} {}",
        type_char(&field("type")),
        field("mode"),
        field("size"),
        field("name")
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    line
}

/// The entry for a file, its symlink target read when it is one. A target
/// that cannot be read is a failure, not an entry without one: `ls` says
/// what is there or says why it cannot.
fn listed(fs: &Filesystem, path: &[u8], name: &[u8], ino: u64) -> Result<Json, CliError> {
    let file = fs.open_ino(ino).map_err(|e| xfs_error(path, e))?;
    let target = if file.is_symlink() {
        Some(file.link_target().map_err(|e| xfs_error(path, e))?)
    } else {
        None
    };
    Ok(entry(name, file.inode(), target))
}

fn ls(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| xfs_error(path, e))?;
    let entries = if file.is_dir() {
        let mut listed_entries = Vec::new();
        for d in file.entries().map_err(|e| xfs_error(path, e))? {
            if d.name == b"." || d.name == b".." {
                continue;
            }
            let mut full = path.to_vec();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(&d.name);
            listed_entries.push(listed(fs, &full, &d.name, d.ino)?);
        }
        listed_entries.sort_by(|a, b| {
            a.get("name")
                .map(Json::to_text)
                .cmp(&b.get("name").map(Json::to_text))
        });
        listed_entries
    } else {
        let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
        vec![listed(fs, path, name, file.inode().ino)?]
    };
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Outcome::report(Json::Arr(entries)).with_text(text))
}

/// Stream a regular file's bytes. Everything that can be refused up front
/// -- no such path, a directory, a symlink, a block map that fails its
/// checksum -- is refused before a byte is written, because the block map
/// is resolved by the first read. Each later chunk is read before it is
/// written, so a device that fails part-way stops with status 1 and what
/// came before stays on stdout. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| xfs_error(path, e))?;
    if file.is_dir() {
        return Err(CliError::failed(format!("{}: is a directory", show(path))));
    }
    if file.is_symlink() {
        let target = file.link_target().map(|t| show(&t)).unwrap_or_default();
        return Err(CliError::failed(format!(
            "{}: is a symlink to {target}; read the target instead",
            show(path)
        )));
    }
    if !file.is_regular_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(path)
        )));
    }
    let size = file.len();
    const CHUNK: usize = 1 << 20;
    let mut buf = vec![0u8; CHUNK.min(usize::try_from(size).unwrap_or(CHUNK)).max(1)];
    // The first chunk is read before anything is opened for writing, so a
    // file whose block map cannot be read leaves no output behind at all.
    let first = if size == 0 {
        0
    } else {
        file.read_at(0, &mut buf).map_err(|e| xfs_error(path, e))?
    };
    let mut copy = |sink: &mut dyn Write| -> Result<(), CliError> {
        let write = |sink: &mut dyn Write, bytes: &[u8]| {
            sink.write_all(bytes)
                .map_err(|e| CliError::failed(format!("write: {e}")))
        };
        write(sink, &buf[..first])?;
        let mut offset = first as u64;
        while offset < size {
            let got = file
                .read_at(offset, &mut buf)
                .map_err(|e| xfs_error(path, e))?;
            if got == 0 {
                return Err(CliError::failed(format!(
                    "{}: short read at byte {offset} of {size}",
                    show(path)
                )));
            }
            write(sink, &buf[..got])?;
            offset += got as u64;
        }
        sink.flush()
            .map_err(|e| CliError::failed(format!("write: {e}")))
    };
    if first == 0 && size != 0 {
        return Err(CliError::failed(format!(
            "{}: short read at byte 0 of {size}",
            show(path)
        )));
    }
    match output {
        None => copy(&mut std::io::stdout().lock())?,
        Some(dest) => {
            let dest = std::path::Path::new(dest);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let mut f = std::fs::File::create(&partial)
                .map_err(|e| CliError::failed(format!("create {}: {e}", partial.display())))?;
            if let Err(e) = copy(&mut f) {
                drop(f);
                let _ = std::fs::remove_file(&partial);
                return Err(e);
            }
            std::fs::rename(&partial, dest)
                .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))?;
        }
    }
    Ok(Outcome::done())
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
