//! `fs.xfs <target> <verb>`: an errand inside an XFS image or device,
//! without mounting it.
//!
//! The verbs are the shared set. Metadata is JSON (or `--text`); file
//! content is raw bytes. A verb the library cannot do yet still exists and
//! answers `not implemented` with exit status 3, so a script moved between
//! filesystems fails loudly instead of meaning something else.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::Write;
use std::sync::{Arc, Mutex};

use clap::{value_parser, Arg, ArgAction, ArgMatches, Command as Cmd};

use fs_core::cli::{CliError, Format as CliFormat, Json, Outcome, Tool};
use fs_core::{BlockDevice, BlockRead};
use fs_xfs::endian::{be16, be32};
use fs_xfs::format::log_items::log_dinode::offsets;
use fs_xfs::fs::File;
use fs_xfs::inode::{FileType, Format, Inode};
use fs_xfs::Filesystem;

pub const TOOL: Tool = Tool {
    name: "fs.xfs",
    verb: "fs",
    section: 1,
    usage_exit: fs_core::cli::output::EXIT_USAGE,
    about: "List, read, write and inspect an XFS image or device without mounting it",
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
        .about("List, read, write and inspect an XFS image or device without mounting it")
        .long_about(
            "Work inside an XFS image or device directly: no mount, no kernel driver.\n\n\
             An escape hatch for an errand (get a file out, read the label, check whether \
             the log needs replaying), not a place to do real filesystem work: for that, \
             mount it.\n\n\
             Metadata is JSON on stdout (--text for people); `read` and a named `xattr` \
             write raw bytes and `write` reads them from stdin. A failure is \
             {\"error\": \"...\", \"code\": N} on stderr, N being the exit status: \
             1 failed, 2 wrong command line, 3 not implemented or refused, 4 refused by \
             the volume's state (rm, rmdir, mv, ln, mkdir or truncate meeting a directory \
             that is not empty, a name already there, a missing target or a part of the \
             path that is not a directory).\n\n\
             Writing is partial, and every other shape is refused by name rather than \
             attempted: a new file (v5, one extent), an empty file given contents, a \
             same-length overwrite in place, bytes written into a file at a position \
             (write --at), mkdir, rm, rmdir, ln, mv and truncate (v5). A journalled \
             change is written home and the log closed with an unmount record before \
             the tool exits, so the volume is clean and the next write can follow at \
             once.",
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
        .args(fs_core::cli::format_args().map(|a| a.global(true)))
        .subcommand_required(true)
        .subcommand(
            Cmd::new("ls")
                .about("List a directory: name, type, size, mode, mtime (and a symlink's target)")
                .arg(
                    Arg::new("recursive")
                        .long("recursive")
                        .short('R')
                        .action(ArgAction::SetTrue)
                        .help("Walk depth first, sorting names as bytes; never follow symlinks"),
                )
                .arg(
                    Arg::new("max-depth")
                        .long("max-depth")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help(
                            "Maximum recursive depth (default: 64); direct children have depth 1",
                        ),
                )
                .arg(
                    Arg::new("limit")
                        .long("limit")
                        .value_name("N")
                        .value_parser(value_parser!(u64))
                        .requires("recursive")
                        .help("Maximum recursive entries (default: 100000)"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .default_value("/")
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img ls /etc\n  \
                     fs.xfs disk.img ls / | jq -r '.[].name'\n  \
                     fs.xfs disk.img ls --text /\n  \
                     fs.xfs disk.img ls -R --max-depth 8 --limit 1000 /etc\n\n\
                     Recursive entries add volume-absolute path and depth. The starting \
                     directory is omitted; a starting file or symlink has depth 0. A repeated \
                     directory inode is listed with revisited: true and not entered. If another \
                     entry would exceed either bound, or a directory cannot be read, the \
                     collected prefix stays on stdout and an error names the bound or path \
                     on stderr (exit 1). Exact fits succeed; zero allows no entries or depth.",
                ),
        )
        .subcommand(
            Cmd::new("stat")
                .about("Report one path: identity, type, mode, owner, links, sizes and every time")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img stat /etc/hostname\n  \
                     fs.xfs disk.img stat /var/log | jq .nlink\n  \
                     fs.xfs disk.img stat --text /bin/sh\n\n\
                     A symlink is reported, not followed. Times are signed seconds since \
                     the epoch plus nanoseconds; creation time is null on v4 inodes.",
                ),
        )
        .subcommand(
            Cmd::new("xattr")
                .about("List extended attribute names and sizes, or read one value's raw bytes")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("name")
                        .value_name("NAME")
                        .value_parser(value_parser!(OsString))
                        .help("The full attribute name, including its namespace prefix"),
                )
                .arg(
                    Arg::new("output")
                        .short('o')
                        .long("output")
                        .value_name("FILE")
                        .requires("name")
                        .value_parser(value_parser!(OsString))
                        .help("Write the named value here instead of stdout"),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img xattr /etc/hostname\n  \
                     fs.xfs disk.img xattr --text /etc/hostname\n  \
                     fs.xfs disk.img xattr /etc/hostname user.comment\n  \
                     fs.xfs disk.img xattr /bin/tool security.capability -o capability.bin\n\n\
                     Without NAME, list {name, size} objects as JSON, or tab-separated \
                     name and byte size with --text. A non-UTF-8 name is shown lossily \
                     with its exact bytes in name_hex. With NAME, write only the value's \
                     raw bytes, including an empty value, regardless of output format. \
                     A missing name exits 1 before opening FILE. Like read, -o writes \
                     FILE.partial and renames it on success. Inspection is read-only; \
                     no symlink is followed and no attribute is set or removed.",
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
        .subcommand(
            Cmd::new("import")
                .about("Copy a local file, stdin or a directory tree into the image")
                .args(super::import::args())
                .after_help(
                    "Examples:\n  fs.xfs disk.img import backup.tar /backup.tar\n  \
                     tar c . | fs.xfs disk.img import - /backup.tar --replace\n  \
                     fs.xfs disk.img import --recursive site /srv/site\n\n\
                     SOURCE is a local file, or - for stdin. The bytes are written in \
                     fixed chunks to a staged file beside PATH, the volume is released \
                     clean, and only then is the staged file renamed to PATH, so PATH is \
                     never half written. An existing PATH is refused unless --replace, \
                     which replaces it in the one rename. A failed import removes the \
                     staged file and leaves PATH as it was. With --recursive every item \
                     is reported as completed, skipped, refused or failed. v5 only; \
                     links, times and sparse files are refused.",
                ),
        )
        .subcommand(
            Cmd::new("export")
                .about("Copy a file, or a directory tree with --recursive, out of the image")
                .args(super::export::args())
                .after_help(
                    "Examples:\n  fs.xfs disk.img export /backup.tar backup.tar\n  \
                     fs.xfs disk.img export --recursive --preserve=times,links /home home\n  \
                     fs.xfs disk.img export /backup.tar - | tar t\n\n\
                     A file is copied in fixed chunks to DEST, a local file published only \
                     once it is complete, or to stdout for -. An existing DEST is refused \
                     unless --replace. With --recursive every item of the tree is reported as \
                     completed, skipped, refused or failed, and the exit status is nonzero \
                     unless all completed. The image is opened read-only.",
                ),
        )
        .subcommand(
            Cmd::new("write")
                .about(
                    "Create a file, or overwrite one of the same length, with the bytes on stdin; \
                     with --at, write them into a file at a byte",
                )
                .arg(
                    Arg::new("at")
                        .long("at")
                        .value_name("BYTES")
                        .value_parser(value_parser!(u64))
                        .help(
                            "Write into the existing file starting at this byte, in place, \
                             without truncating it (not --offset, which is where the \
                             filesystem starts in TARGET)",
                        ),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img write /notes.txt < notes.txt\n  \
                     fs.xfs src.img read /f | fs.xfs dst.img write /f\n  \
                     fs.xfs disk.img write --at 4096 /data.bin < patch.bin\n\n\
                     A new path becomes a file in one extent (v5 only). An empty file is \
                     given its contents. An existing file of exactly the same length is \
                     overwritten in place. Anything else -- appending, shortening, a v4 \
                     volume, a file too large for one free run -- is refused with the \
                     reason (exit 3), and the volume is left as it was.\n\n\
                     --at writes stdin into an existing regular file starting at that \
                     byte, and nothing else of it changes: not its size, not its blocks. \
                     Writing past the end or into a hole or an unwritten extent is \
                     refused (exit 3); so are a missing file, a directory and a symlink \
                     (exit 1). Every refusal a read can decide is made before the image \
                     is opened for writing. The report is \
                     {\"path\": ..., \"at\": N, \"bytes\": N, \"size\": N}.",
                ),
        )
        .subcommand(
            Cmd::new("mkdir")
                .about("Create a directory (v5); --parents creates its missing parents too")
                .arg(
                    Arg::new("parents")
                        .long("parents")
                        .short('p')
                        .action(ArgAction::SetTrue)
                        .help("Create missing directories; an existing directory is success"),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img mkdir /backup\n  \
                     fs.xfs disk.img mkdir /backup/2026\n  \
                     fs.xfs disk.img mkdir -p /backup/2026/october\n\n\
                     Without --parents, the parent must exist and PATH must be new. \
                     With --parents, the report lists only directories created, as \
                     {\"path\": PATH, \"created\": [PATH, ...]}; an existing chain creates \
                     nothing and needs no writable open. Created paths are absolute. \
                     A file or symlink anywhere on the way, or an invalid name, is \
                     refused before anything is created; symlinks are never followed. \
                     Every missing directory is made on one writable mount. \
                     The change is journalled, then written home with the log closed \
                     behind it, so the volume is left clean for the next write.",
                ),
        )
        .subcommand(
            Cmd::new("rm")
                .about("Remove a name that is not a directory; the file goes with its last name")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img rm /notes.txt\n  \
                     fs.xfs disk.img rm --text /backup/old.tar\n\n\
                     A symlink is removed, never followed, and no symlink on the way to \
                     PATH is followed either. A file with other names keeps its content \
                     under them. A missing path, a directory, a v4 volume and a log \
                     holding records are refused before the image is opened for writing. \
                     The volume is released clean, so the next write need not wait for \
                     Linux. Nothing is removed recursively.",
                ),
        )
        .subcommand(
            Cmd::new("rmdir")
                .about("Remove an empty directory")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img rmdir /backup\n  \
                     fs.xfs disk.img rmdir --text /backup/2026\n\n\
                     A missing path, anything that is not a directory, a directory with \
                     entries, the root, a v4 volume and a log holding records are refused \
                     before the image is opened for writing. The volume is released clean.",
                ),
        )
        .subcommand(
            Cmd::new("ln")
                .about("Add a hard link, or with -s make a symbolic link (v5 only)")
                .arg(
                    Arg::new("symbolic")
                        .short('s')
                        .long("symbolic")
                        .action(ArgAction::SetTrue)
                        .help("Store TARGETTEXT exactly as given, without resolving it"),
                )
                .arg(
                    Arg::new("link-target")
                        .value_name("TARGETPATH|TARGETTEXT")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img ln /data/a /data/also-a\n  \
                     fs.xfs disk.img ln -s ../etc/hosts /hosts\n\n\
                     PATH must be new and its parent must exist. A hard link names the \
                     target inode (a symlink is not followed); directories and the maximum \
                     link count are refused. A symbolic target is literal text, 1 to 1023 \
                     bytes, and need not exist. No parent symlinks are followed. Refusals \
                     are checked read-only, and the journalled edit is released clean.",
                ),
        )
        .subcommand(
            Cmd::new("mv")
                .about("Give a name a new one, in the same directory or another")
                .arg(
                    Arg::new("source")
                        .value_name("SOURCE")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img mv /notes.txt /archive/notes.txt\n  \
                     fs.xfs disk.img mv --text /draft /final\n\n\
                     PATH is the new name itself, not a directory to move into. A name \
                     already at PATH is replaced, as rename(2) replaces it: a file by a \
                     file, an empty directory by a directory. A symlink is moved, never \
                     followed, and no symlink on the way to either path is followed. A \
                     missing SOURCE or parent, a directory moved beneath itself, the root, \
                     `.`, `..`, a v4 volume and a log holding records are refused before \
                     the image is opened for writing; a replacement the driver does not \
                     make is refused with its reason (exit 3) before anything is written. \
                     The volume is released clean.",
                ),
        )
        .subcommand(
            Cmd::new("truncate")
                .about("Set a regular file's length, freeing blocks past a shorter end (v5)")
                .arg(
                    Arg::new("path")
                        .value_name("PATH")
                        .required(true)
                        .value_parser(value_parser!(OsString)),
                )
                .arg(
                    Arg::new("size")
                        .value_name("SIZE")
                        .required(true)
                        .value_parser(value_parser!(u64)),
                )
                .after_help(
                    "Examples:\n  fs.xfs disk.img truncate /log.txt 0\n  \
                     fs.xfs disk.img truncate /sparse.img 1073741824\n\n\
                     SIZE is a byte count. Shrinking frees blocks past the new end; \
                     growing reads back as zeros without allocating blocks. Symlinks \
                     are never followed. A v4 volume, a shared partial last block, \
                     and a cut whose remaining extents still need a B+tree are refused \
                     (exit 3). Every driver refusal is checked in memory before TARGET \
                     is opened for writing. The journalled edit is released clean.",
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
                .about("Change a property: the label")
                .arg(Arg::new("key").value_name("KEY").required(true))
                .arg(Arg::new("value").value_name("VALUE").required(true))
                .after_help(
                    "Examples:\n  fs.xfs disk.img set label BACKUP\n\n\
                     The label is written to the superblock of every allocation group; \
                     at most 12 bytes. A volume whose log is not clean is refused.",
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
             fs.xfs disk.img write /notes.txt < notes.txt\n  \
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
        "ls" if sub.get_flag("recursive") => {
            let format = CliFormat::of(matches, &std::env::args_os().collect::<Vec<_>>());
            ls_recursive(
                &super::device::mount(target, offset)?,
                path_arg(sub),
                sub,
                format,
            )
        }
        "ls" => ls(&super::device::mount(target, offset)?, path_arg(sub)),
        "stat" => stat(&super::device::mount(target, offset)?, path_arg(sub)),
        "xattr" => xattr(
            &super::device::mount(target, offset)?,
            path_arg(sub),
            sub.get_one::<OsString>("name").map(os_bytes),
            sub.get_one("output"),
        ),
        "read" => read(
            &super::device::mount(target, offset)?,
            path_arg(sub),
            sub.get_one("output"),
        ),
        "import" => super::import::import(target, offset, sub),
        "export" => super::export::export(&super::device::mount(target, offset)?, target, sub),
        "write" => match sub.get_one::<u64>("at") {
            Some(&at) => write_at(target, offset, path_arg(sub), at),
            None => write(target, offset, path_arg(sub)),
        },
        "mkdir" if sub.get_flag("parents") => mkdir_parents(target, offset, path_arg(sub)),
        "mkdir" => mkdir(target, offset, path_arg(sub)),
        "truncate" => truncate(
            target,
            offset,
            path_arg(sub),
            *sub.get_one::<u64>("size").expect("clap requires the size"),
        ),
        "rm" => remove(target, offset, path_arg(sub), Removal::Name),
        "rmdir" => remove(target, offset, path_arg(sub), Removal::Directory),
        "ln" => ln(
            target,
            offset,
            os_bytes(
                sub.get_one::<OsString>("link-target")
                    .expect("required target"),
            ),
            path_arg(sub),
            sub.get_flag("symbolic"),
        ),
        "mv" => mv(
            target,
            offset,
            os_bytes(
                sub.get_one::<OsString>("source")
                    .expect("clap requires the source"),
            ),
            path_arg(sub),
        ),
        "get" | "info" => get(
            &super::device::mount(target, offset)?,
            sub.get_one::<String>("key").map(String::as_str),
        ),
        "set" => set(target, offset, sub),
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

pub(super) fn show(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(super) fn xfs_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    CliError::failed(format!("{}: {e}", show(what)))
}

/// A reading verb's open: a path that does not exist is rust-fs-core's
/// `not_found` kind, exit 1, which a script matches whatever the message
/// says; every other error is the driver's.
pub(super) fn read_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    match e {
        fs_xfs::Error::NotFound => CliError::not_found(format!("{}: {e}", show(what))),
        e => xfs_error(what, e),
    }
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
        e.get("path")
            .map(Json::to_text)
            .unwrap_or_else(|| field("name"))
    );
    if let Some(target) = e.get("target") {
        line.push_str(&format!(" -> {}", target.to_text()));
    }
    if e.get("revisited") == Some(&Json::Bool(true)) {
        line.push_str(" [revisited]");
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
    let file = fs.open_bytes(path).map_err(|e| read_error(path, e))?;
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

/// Names in bit order, with any unrecognised bits retained as hex.
fn inode_flag_names(mut bits: u64, names: &[&str]) -> Json {
    let mut flags = Vec::new();
    for (bit, name) in names.iter().enumerate() {
        let mask = 1 << bit;
        if bits & mask != 0 {
            flags.push(Json::from(*name));
            bits &= !mask;
        }
    }
    if bits != 0 {
        flags.push(Json::from(format!("0x{bits:x}")));
    }
    Json::Arr(flags)
}

/// The shared `stat` keys and ordering match the other filesystem tools.
/// XFS's inode layout is documented in XFS Filesystem Structure, "On-disk
/// Inode"; cli_stat_kernel holds the decoded values to Linux and xfsprogs.
fn stat(fs: &Filesystem, path: &[u8]) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| read_error(path, e))?;
    let inode = file.inode();
    let raw = file.raw();
    let v3 = inode.version == 3;
    let wide_project =
        fs.superblock().features2 & fs_xfs::superblock::features2_flags::PROJID32BIT != 0;
    let projid = u32::from(be16(raw, offsets::PROJID_LO))
        | if wide_project {
            u32::from(be16(raw, offsets::PROJID_HI)) << 16
        } else {
            0
        };
    let allocated = inode
        .nblocks
        .checked_mul(u64::from(fs.superblock().blocksize))
        .ok_or_else(|| CliError::failed("allocated byte count overflows"))?;
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.extend([
        ("inode", Json::from(inode.ino)),
        ("type", Json::from(type_name(inode.file_type()))),
        ("mode", Json::from(format!("{:04o}", inode.mode & 0o7777))),
        ("uid", Json::from(inode.uid)),
        ("gid", Json::from(inode.gid)),
        ("nlink", Json::from(inode.nlink)),
        ("size", Json::from(inode.size)),
        ("allocated_bytes", Json::from(allocated)),
        ("atime", Json::from(inode.atime.sec)),
        ("atime_nsec", Json::from(inode.atime.nsec)),
        ("mtime", Json::from(inode.mtime.sec)),
        ("mtime_nsec", Json::from(inode.mtime.nsec)),
        ("ctime", Json::from(inode.ctime.sec)),
        ("ctime_nsec", Json::from(inode.ctime.nsec)),
        ("btime", Json::from(v3.then_some(inode.crtime.sec))),
        ("btime_nsec", Json::from(v3.then_some(inode.crtime.nsec))),
    ]);
    match inode.file_type() {
        Some(FileType::Symlink) => fields.push((
            "target",
            Json::from(show(&file.link_target().map_err(|e| xfs_error(path, e))?)),
        )),
        Some(FileType::CharDevice | FileType::BlockDevice) => {
            // XFS's on-disk SysV device number: 14 major bits, 18 minor
            // bits. The wide node in cli_stat_kernel distinguishes this
            // from Linux's in-memory dev_t and its other disk encodings.
            let dev = be32(raw, inode.data_fork_offset());
            fields.push(("major", Json::from(dev >> 18)));
            fields.push(("minor", Json::from(dev & 0x3_ffff)));
        }
        _ => {}
    }
    fields.push((
        "xfs",
        Json::object([
            ("version", Json::from(u32::from(inode.version))),
            (
                "flags",
                inode_flag_names(
                    u64::from(inode.flags),
                    &[
                        "REALTIME",
                        "PREALLOC",
                        "NEWRTBM",
                        "IMMUTABLE",
                        "APPEND",
                        "SYNC",
                        "NOATIME",
                        "NODUMP",
                        "RTINHERIT",
                        "PROJINHERIT",
                        "NOSYMLINKS",
                        "EXTSIZE",
                        "EXTSZINHERIT",
                        "NODEFRAG",
                        "FILESTREAM",
                    ],
                ),
            ),
            (
                "flags2",
                Json::from(v3.then(|| {
                    inode_flag_names(
                        inode.flags2,
                        &["DAX", "REFLINK", "COWEXTSIZE", "BIGTIME", "NREXT64"],
                    )
                })),
            ),
            ("generation", Json::from(inode.gen)),
            ("projid", Json::from((inode.version >= 2).then_some(projid))),
            ("extsize", Json::from(be32(raw, offsets::EXTSIZE))),
            ("nextents", Json::from(inode.nextents)),
            ("anextents", Json::from(inode.anextents)),
            ("crtime", Json::from(v3.then_some(inode.crtime.sec))),
            ("crtime_nsec", Json::from(v3.then_some(inode.crtime.nsec))),
            (
                "bigtime",
                Json::from(v3.then_some(inode.flags2 & fs_xfs::inode::flags2::BIGTIME != 0)),
            ),
            (
                "format",
                Json::from(match inode.format {
                    Format::Dev => "dev",
                    Format::Local => "local",
                    Format::Extents => "extents",
                    Format::Btree => "btree",
                    Format::Uuid => "uuid",
                    Format::Rmap => "rmap",
                }),
            ),
        ]),
    ));
    Ok(Outcome::report(Json::object(fields)))
}

fn recursive_entry(
    file: &File<'_>,
    path: &[u8],
    depth: u64,
    revisited: bool,
) -> Result<Json, CliError> {
    let name = path.rsplit(|b| *b == b'/').next().unwrap_or(path);
    let target = if file.is_symlink() {
        Some(file.link_target().map_err(|e| xfs_error(path, e))?)
    } else {
        None
    };
    let Json::Obj(mut fields) = entry(name, file.inode(), target) else {
        unreachable!("an ls entry is an object")
    };
    fields.push(("path".into(), Json::from(show(path))));
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex".into(),
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields.push(("depth".into(), Json::from(depth)));
    if revisited {
        fields.push(("revisited".into(), Json::from(true)));
    }
    Ok(Json::Obj(fields))
}

/// Directory iterators live on a heap stack, so a caller-supplied large
/// depth bound cannot overflow Rust's call stack. The starting inode is
/// visited before descent; every other directory identity is entered once.
fn ls_recursive(
    fs: &Filesystem,
    path: &[u8],
    sub: &ArgMatches,
    format: CliFormat,
) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| read_error(path, e))?;
    let max_depth = sub.get_one::<u64>("max-depth").copied().unwrap_or(64);
    let limit = sub.get_one::<u64>("limit").copied().unwrap_or(100_000);
    let mut start = Vec::new();
    for component in path
        .split(|b| *b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
    {
        start.push(b'/');
        start.extend_from_slice(component);
    }
    if start.is_empty() {
        start.push(b'/');
    }
    let mut entries = Vec::new();
    let walked = (|| {
        if !file.is_dir() {
            if limit == 0 {
                return Err(CliError::failed(format!(
                    "--limit 0 reached at {}",
                    show(&start)
                )));
            }
            entries.push(recursive_entry(&file, &start, 0, false)?);
            return Ok(());
        }
        let children = |file: &File<'_>, at: &[u8]| {
            let mut children = file.entries().map_err(|e| xfs_error(at, e))?;
            children.retain(|d| d.name != b"." && d.name != b"..");
            children.sort_by(|a, b| a.name.cmp(&b.name));
            Ok::<_, CliError>(children.into_iter())
        };
        let mut visited = HashSet::from([file.inode().ino]);
        let first = children(&file, &start)?;
        let mut stack = vec![(start, 0u64, first)];
        while let Some((parent, parent_depth, children_iter)) = stack.last_mut() {
            let Some(child) = children_iter.next() else {
                stack.pop();
                continue;
            };
            let mut full = parent.clone();
            if !full.ends_with(b"/") {
                full.push(b'/');
            }
            full.extend_from_slice(&child.name);
            let depth = *parent_depth + 1;
            if entries.len() as u64 >= limit {
                return Err(CliError::failed(format!(
                    "--limit {limit} reached at {}",
                    show(&full)
                )));
            }
            if depth > max_depth {
                return Err(CliError::failed(format!(
                    "--max-depth {max_depth} reached at {}",
                    show(&full)
                )));
            }
            let file = fs.open_ino(child.ino).map_err(|e| xfs_error(&full, e))?;
            let revisited = file.is_dir() && !visited.insert(file.inode().ino);
            entries.push(recursive_entry(&file, &full, depth, revisited)?);
            if file.is_dir() && !revisited {
                let next = children(&file, &full)?;
                stack.push((full, depth, next));
            }
        }
        Ok(())
    })();
    let text = entries
        .iter()
        .map(entry_text)
        .collect::<Vec<_>>()
        .join("\n");
    let report = Outcome::report(Json::Arr(entries)).with_text(text);
    match walked {
        Ok(()) => Ok(report),
        Err(error) => {
            // Outcome holds one stream: render the complete partial report
            // here, then let the shared dispatcher render the error.
            let partial = fs_core::cli::output::render(TOOL.name, format, Ok(report));
            let mut out = std::io::stdout().lock();
            out.write_all(partial.stdout.as_bytes())
                .and_then(|()| out.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
            Err(error)
        }
    }
}

/// Inspect attributes on a read-only mount. Read the whole value before
/// opening an output, so a missing attribute or a damaged fork emits no
/// content and leaves an existing destination alone.
fn xattr(
    fs: &Filesystem,
    path: &[u8],
    name: Option<&[u8]>,
    output: Option<&OsString>,
) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| xfs_error(path, e))?;
    if let Some(name) = name {
        let value = fs
            .get_xattr(file.inode(), file.raw(), name)
            .map_err(|e| xfs_error(path, e))?
            .ok_or_else(|| {
                CliError::failed(format!("{}: no such attribute: {}", show(path), show(name)))
            })?;
        if let Some(dest) = output {
            let dest = std::path::Path::new(dest);
            let mut partial = dest.as_os_str().to_owned();
            partial.push(".partial");
            let partial = std::path::PathBuf::from(partial);
            let written = (|| {
                std::fs::write(&partial, &value)
                    .map_err(|e| CliError::failed(format!("write {}: {e}", partial.display())))?;
                std::fs::rename(&partial, dest)
                    .map_err(|e| CliError::failed(format!("rename to {}: {e}", dest.display())))
            })();
            if written.is_err() {
                let _ = std::fs::remove_file(&partial);
            }
            written?;
        } else {
            let mut stdout = std::io::stdout().lock();
            stdout
                .write_all(&value)
                .and_then(|()| stdout.flush())
                .map_err(|e| CliError::failed(format!("write: {e}")))?;
        }
        return Ok(Outcome::done());
    }
    let mut attrs = fs
        .list_xattrs(file.inode(), file.raw())
        .map_err(|e| xfs_error(path, e))?;
    attrs.sort_by(|a, b| a.name.cmp(&b.name));
    let text = attrs
        .iter()
        .map(|a| format!("{}\t{}", show(&a.name), a.value.len()))
        .collect::<Vec<_>>()
        .join("\n");
    let rows = attrs
        .into_iter()
        .map(|a| {
            let mut fields = vec![("name", Json::from(show(&a.name)))];
            if std::str::from_utf8(&a.name).is_err() {
                fields.push((
                    "name_hex",
                    Json::from(
                        a.name
                            .iter()
                            .map(|b| format!("{b:02x}"))
                            .collect::<String>(),
                    ),
                ));
            }
            fields.push(("size", Json::from(a.value.len() as u64)));
            Json::object(fields)
        })
        .collect();
    Ok(Outcome::report(Json::Arr(rows)).with_text(text))
}

/// Stream a regular file's bytes. Everything that can be refused up front
/// -- no such path, a directory, a symlink, a block map that fails its
/// checksum -- is refused before a byte is written, because the block map
/// is resolved by the first read. Each later chunk is read before it is
/// written, so a device that fails part-way stops with status 1 and what
/// came before stays on stdout. `-o FILE` writes `FILE.partial` and renames
/// it, so FILE is never left half written.
fn read(fs: &Filesystem, path: &[u8], output: Option<&OsString>) -> Result<Outcome, CliError> {
    let file = fs.open_bytes(path).map_err(|e| read_error(path, e))?;
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

/// What a new file and a new directory are made with. The library takes
/// the whole `di_mode`, type bits included: a bare `0o644` makes an inode
/// of no type at all, which the kernel and xfs_repair reject.
pub(super) const FILE_MODE: u16 = 0o100_644;
pub(super) const DIR_MODE: u16 = 0o040_755;

/// A driver error from a write verb: a shape the driver refuses is exit 3
/// with the driver's own reason, anything else a failure.
pub(super) fn write_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    match e {
        fs_xfs::Error::UnsupportedFeature(_) | fs_xfs::Error::ReadOnly => {
            CliError::refused(format!("{}: {e}", show(what)))
        }
        other => xfs_error(what, other),
    }
}

/// A namespace verb's refusal by the volume's current state -- a name that
/// is not there, a name already taken, a part of the path that is not a
/// directory, a directory that is not empty -- is the CLI contract's exit 4,
/// `refused` (rust-fs-core's `EXIT_REFUSED`). `Err` hands every other error
/// back.
fn state_refusal(what: &[u8], e: fs_xfs::Error) -> Result<CliError, fs_xfs::Error> {
    use fs_xfs::Error;
    match e {
        Error::NotFound
        | Error::AlreadyExists
        | Error::NotADirectory
        | Error::DirectoryNotEmpty => {
            Ok(CliError::refused_by_state(format!("{}: {e}", show(what))))
        }
        other => Err(other),
    }
}

/// [`xfs_error`] for a namespace verb: its state refusals are exit 4.
pub(super) fn state_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    state_refusal(what, e).unwrap_or_else(|e| xfs_error(what, e))
}

/// [`write_error`] for a namespace verb: its state refusals are exit 4.
pub(super) fn state_write_error(what: &[u8], e: fs_xfs::Error) -> CliError {
    state_refusal(what, e).unwrap_or_else(|e| write_error(what, e))
}

/// Release a volume a journalled verb has changed, clean: `unmount` writes
/// the change home, puts the summary counters right and ends the log with
/// an unmount record (#439), so `get dirty` is false afterwards and the
/// next write need not wait for Linux to replay it.
pub(super) fn release_clean(fs: Filesystem, what: &[u8]) -> Result<(), CliError> {
    fs.unmount().map_err(|e| {
        CliError::failed(format!(
            "{}: changed in the log, but the volume could not be released clean: {e}",
            show(what)
        ))
    })
}

/// `path` split into its parent directory and its last component, which
/// must be a name: not empty, `.` or `..`.
pub(super) fn split_path(path: &[u8]) -> Result<(&[u8], &[u8]), CliError> {
    let trimmed = {
        let mut end = path.len();
        while end > 1 && path[end - 1] == b'/' {
            end -= 1;
        }
        &path[..end]
    };
    let (parent, name) = match trimmed.iter().rposition(|b| *b == b'/') {
        Some(at) => (&trimmed[..at.max(1)], &trimmed[at + 1..]),
        None => (&b"/"[..], trimmed),
    };
    if name.is_empty() || name == b"." || name == b".." {
        return Err(CliError::usage(format!(
            "{}: names no file to create",
            show(path)
        )));
    }
    Ok((parent, name))
}

/// Replace a file's contents, or make it, with everything on stdin, in
/// one of the shapes the driver can write -- and refuse the rest, naming
/// why, before the volume is touched.
///
/// The whole input is read before the image is opened, so a failing
/// producer (`false | fs.xfs img write /f`) leaves the image as it was.
///
/// - **A new path**: `create_file`, then `write_into_empty_file` (both
///   journalled, v5 only). If the second is refused -- no single free run
///   long enough, a file too large for one extent -- the new file is
///   unlinked again in the same mount, so a refused write leaves no file
///   behind, and the volume is released clean as after a write.
/// - **An existing empty file**: `write_into_empty_file`.
/// - **An existing file of the same length**: `write_at` in place, which
///   changes no metadata and needs no journal.
/// - **Longer**: `write_at` is asked, and refuses (it would grow the file);
///   its reason is the error.
/// - **Shorter**: refused here. Replacing means truncating and
///   reallocating, which the driver does not do as one operation.
///
/// A journalled write then releases the volume clean, so the next one
/// follows on the volume this one left.
fn write(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let (parent_path, name) = split_path(path)?;
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut data)
        .map_err(|e| CliError::failed(format!("read stdin: {e}")))?;

    let fs = super::device::mount_rw(target, offset)?;
    let parent = fs
        .open_bytes(parent_path)
        .map_err(|e| xfs_error(parent_path, e))?;
    if !parent.is_dir() {
        return Err(xfs_error(parent_path, fs_xfs::Error::NotADirectory));
    }
    let parent_ino = parent.inode().ino;
    let existing = match parent.open_child(name) {
        Ok(file) => Some(file),
        Err(fs_xfs::Error::NotFound) => None,
        Err(e) => return Err(xfs_error(path, e)),
    };

    let (how, bytes) = match existing {
        None => {
            let (ino, _) = fs
                .create_file(parent_ino, name, FILE_MODE)
                .map_err(|e| write_error(path, e))?;
            if !data.is_empty() {
                if let Err(e) = fs.write_into_empty_file(ino, &data) {
                    let refused = write_error(path, e);
                    if let Err(undo) = fs.unlink_file(parent_ino, name) {
                        return Err(CliError::failed(format!(
                            "{} (and removing the file it had created failed too: {undo})",
                            refused.message
                        )));
                    }
                    return Err(match release_clean(fs, path) {
                        Ok(()) => refused,
                        Err(release) => CliError::failed(format!(
                            "{} (and then {})",
                            refused.message, release.message
                        )),
                    });
                }
            }
            ("created", data.len())
        }
        Some(file) => {
            if file.is_dir() {
                return Err(CliError::failed(format!("{}: is a directory", show(path))));
            }
            if !file.is_regular_file() {
                return Err(xfs_error(path, fs_xfs::Error::NotAFile));
            }
            let size = file.len();
            let want = data.len() as u64;
            if want == size {
                if !data.is_empty() {
                    fs.write_at(file.inode(), file.raw(), 0, &data)
                        .map_err(|e| write_error(path, e))?;
                }
                ("overwritten", data.len())
            } else if size == 0 {
                fs.write_into_empty_file(file.inode().ino, &data)
                    .map_err(|e| write_error(path, e))?;
                ("filled", data.len())
            } else if want > size {
                // The driver's own refusal is the answer: it names why a
                // longer write is a metadata change it will not make.
                fs.write_at(file.inode(), file.raw(), 0, &data)
                    .map_err(|e| write_error(path, e))?;
                return Err(CliError::failed(format!(
                    "{}: the driver accepted a write past the end of the file, which it \
                     documents as refused",
                    show(path)
                )));
            } else {
                return Err(CliError::not_implemented(format!(
                    "write: {} holds {size} bytes and stdin {want}; replacing a file with \
                     a shorter one means truncating it and allocating again, which this \
                     driver does not do as one operation. A new path, an empty file, or \
                     exactly {size} bytes can be written",
                    show(path)
                )));
            }
        }
    };
    let journalled = how != "overwritten";
    if journalled {
        release_clean(fs, path)?;
    }
    let report = Json::object([
        ("path", Json::from(show(path))),
        ("bytes", Json::from(bytes as u64)),
        ("result", Json::from(how)),
        ("created", Json::from(how == "created")),
        ("journalled", Json::from(journalled)),
    ]);
    let text = format!("{how} {} ({bytes} bytes)", show(path));
    Ok(Outcome::report(report).with_text(text))
}

/// Write everything on stdin into the existing regular file at `path`,
/// starting at byte `at`, in place: nothing is truncated, grown or
/// allocated.
///
/// The whole input is read before the image is opened, so a failing
/// producer leaves the image as it was. Everything the write could be
/// refused for that a read can decide is then decided on a read-only
/// mount ([`in_place_range`]), so such a refusal never opens the image for
/// writing. The write itself is the library's `write_at`, which checks
/// again and refuses the shapes it does not write in place (a reflinked
/// extent, for one) before it writes anything.
///
/// It changes data, not metadata, so it logs no record: the log is left as
/// clean as the writable mount required it to be, with nothing to release.
/// An empty input writes nothing and never opens the image for writing.
fn write_at(target: &OsString, offset: u64, path: &[u8], at: u64) -> Result<Outcome, CliError> {
    let mut data = Vec::new();
    std::io::Read::read_to_end(&mut std::io::stdin().lock(), &mut data)
        .map_err(|e| CliError::failed(format!("read stdin: {e}")))?;
    let (ino, size) = in_place_range(
        &super::device::mount(target, offset)?,
        target,
        path,
        at,
        data.len() as u64,
    )?;
    if !data.is_empty() {
        let fs = super::device::mount_rw(target, offset)?;
        let file = fs.open_ino(ino).map_err(|e| xfs_error(path, e))?;
        fs.write_at(file.inode(), file.raw(), at, &data)
            .map_err(|e| write_error(path, e))?;
    }
    let mut fields = named(path);
    fields.extend([
        ("at", Json::from(at)),
        ("bytes", Json::from(data.len() as u64)),
        ("size", Json::from(size)),
    ]);
    Ok(Outcome::report(Json::object(fields)).with_text(format!(
        "wrote {} bytes into {} at byte {at}",
        data.len(),
        show(path)
    )))
}

/// The inode and size of the regular file at `path`, once each refusal a
/// read can decide for writing `len` bytes into it at `at` has been ruled
/// out: a log holding records, a path that does not resolve, a directory,
/// a name ending in `/`, a symlink (never followed), anything else that is
/// not a regular file, and then the shapes an in-place write cannot take,
/// which are exit 3: a range reaching past the end of the file, and a
/// block under it that is a hole or in an unwritten extent, since writing
/// either would change the extent list.
fn in_place_range(
    fs: &Filesystem,
    target: &OsString,
    path: &[u8],
    at: u64,
    len: u64,
) -> Result<(u64, u64), CliError> {
    if fs.was_replayed() {
        return Err(super::device::dirty_log(&target.to_string_lossy()));
    }
    let file = fs.open_bytes(path).map_err(|e| xfs_error(path, e))?;
    if file.is_dir() {
        return Err(CliError::failed(format!("{}: is a directory", show(path))));
    }
    if last_name(path).2 {
        return Err(xfs_error(path, fs_xfs::Error::NotADirectory));
    }
    if file.is_symlink() {
        let target = file.link_target().map(|t| show(&t)).unwrap_or_default();
        return Err(CliError::failed(format!(
            "{}: is a symlink to {target}; write to the target instead",
            show(path)
        )));
    }
    if !file.is_regular_file() {
        return Err(xfs_error(path, fs_xfs::Error::NotAFile));
    }
    let size = file.len();
    let end = at
        .checked_add(len)
        .filter(|end| *end <= size)
        .ok_or_else(|| {
            CliError::refused(format!(
                "{}: {len} bytes at byte {at} reach past the end of its {size} bytes, and \
                 write --at writes in place: it neither grows a file nor allocates",
                show(path)
            ))
        })?;
    if len > 0 {
        let extents = fs
            .data_extents(file.inode(), file.raw())
            .map_err(|e| write_error(path, e))?;
        let block = u64::from(fs.superblock().blocksize);
        for file_block in at / block..=(end - 1) / block {
            match fs_xfs::extent::lookup(&extents, file_block) {
                None => {
                    return Err(CliError::refused(format!(
                        "{}: file block {file_block} is a hole, and filling it would \
                         allocate blocks, which write --at does not do",
                        show(path)
                    )))
                }
                Some(extent) if extent.is_unwritten() => {
                    return Err(CliError::refused(format!(
                        "{}: file block {file_block} is in an unwritten extent, and \
                         writing it would change the extent list, which write --at \
                         does not do",
                        show(path)
                    )))
                }
                Some(_) => {}
            }
        }
    }
    Ok((file.inode().ino, size))
}

/// Create one directory, mode 0755. Its parent must exist, and nothing
/// may be at the path yet. The volume is then released clean.
fn mkdir(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    let (parent_path, name) = split_path(path)?;
    let fs = super::device::mount_rw(target, offset)?;
    let parent = fs
        .open_bytes(parent_path)
        .map_err(|e| state_error(parent_path, e))?;
    if !parent.is_dir() {
        return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
    }
    let (ino, _) = fs
        .create_directory(parent.inode().ino, name, DIR_MODE)
        .map_err(|e| state_write_error(path, e))?;
    release_clean(fs, path)?;
    let report = Json::object([("path", Json::from(show(path))), ("inode", Json::from(ino))]);
    Ok(Outcome::report(report).with_text(format!("created {}", show(path))))
}

/// Add a name through the library's journalled link or symlink operation.
/// Resolve and validate on a read-only mount before opening for writing.
fn ln(
    target: &OsString,
    offset: u64,
    source: &[u8],
    path: &[u8],
    symbolic: bool,
) -> Result<Outcome, CliError> {
    let (parent_path, name) = split_path(path)?;
    if path.ends_with(b"/") {
        return Err(state_error(path, fs_xfs::Error::NotADirectory));
    }
    if name.len() > usize::from(u8::MAX) || name.contains(&0) {
        return Err(CliError::refused(
            "ln: the new name must be 1 to 255 bytes with no NUL",
        ));
    }
    let (parent_ino, source_ino) = {
        let fs = super::device::mount(target, offset)?;
        if !fs.superblock().is_v5() {
            return Err(CliError::refused(
                "ln: linking writes v5 metadata; a v4 filesystem is not supported",
            ));
        }
        if fs.was_replayed() {
            return Err(super::device::dirty_log(&target.to_string_lossy()));
        }
        let parent = fs
            .open_bytes(parent_path)
            .map_err(|e| state_write_error(path, e))?;
        if !parent.is_dir() {
            return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
        }
        match parent.open_child(name) {
            Ok(_) => return Err(state_error(path, fs_xfs::Error::AlreadyExists)),
            Err(fs_xfs::Error::NotFound) => {}
            Err(e) => return Err(state_error(path, e)),
        }
        let ino = if symbolic {
            let max = fs_xfs::format::symlink::XFS_SYMLINK_MAX_TARGET;
            if source.is_empty() || source.len() > max || source.contains(&0) {
                return Err(CliError::refused(format!(
                    "ln: a symlink's target is 1 to {max} bytes with no NUL"
                )));
            }
            None
        } else {
            let file = fs
                .open_bytes(source)
                .map_err(|e| state_write_error(source, e))?;
            let inode = file.inode();
            if source.ends_with(b"/") && !file.is_dir() {
                return Err(state_error(source, fs_xfs::Error::NotADirectory));
            }
            if file.is_dir() {
                return Err(CliError::refused(format!(
                    "{}: a directory cannot be given another hard link",
                    show(source)
                )));
            }
            if inode.nlink == 0 {
                return Err(state_error(source, fs_xfs::Error::NotFound));
            }
            if inode.nlink >= (1 << 31) - 1 {
                return Err(CliError::refused(format!(
                    "{}: inode {} already has {} links, the most it may have",
                    show(source),
                    inode.ino,
                    inode.nlink
                )));
            }
            Some(inode.ino)
        };
        (parent.inode().ino, ino)
    };
    let fs = super::device::mount_rw(target, offset)?;
    let ino = match source_ino {
        Some(ino) => {
            fs.link(ino, parent_ino, name)
                .map_err(|e| state_write_error(path, e))?;
            ino
        }
        None => {
            fs.create_symlink(parent_ino, name, source)
                .map_err(|e| state_write_error(path, e))?
                .0
        }
    };
    release_clean(fs, path)?;
    Ok(Outcome::report(Json::object([
        ("path", Json::from(show(path))),
        ("linked", Json::from(true)),
        ("inode", Json::from(ino)),
    ]))
    .with_text(format!("linked {}", show(path))))
}

/// Plan the entire walk read-only, including components after `..` that
/// return from a missing branch to an existing one. Planned directories
/// exist virtually during this walk, so no path refusal leaves a partial chain.
/// The actual creates share one writable mount and one clean release.
fn mkdir_parents(target: &OsString, offset: u64, path: &[u8]) -> Result<Outcome, CliError> {
    if path.is_empty() {
        return Err(CliError::usage("mkdir --parents: PATH must not be empty"));
    }
    let fs = super::device::mount(target, offset)?;
    let mut planned = HashSet::new();
    let mut missing = Vec::new();
    let mut current = b"/".to_vec();
    for name in path.split(|&b| b == b'/') {
        match name {
            b"" | b"." => continue,
            b".." => {
                let slash = current.iter().rposition(|&b| b == b'/').unwrap_or(0);
                current.truncate(slash.max(1));
                continue;
            }
            _ => {}
        }
        if name.len() > usize::from(u8::MAX) || name.contains(&0) {
            return Err(CliError::refused(format!(
                "{}: not a name a directory entry can hold",
                show(name)
            )));
        }
        if current.len() > 1 {
            current.push(b'/');
        }
        current.extend_from_slice(name);
        if planned.contains(&current) {
            continue;
        }
        match fs.open_bytes(&current) {
            Ok(file) if file.is_dir() => {}
            Ok(_) => return Err(state_error(&current, fs_xfs::Error::NotADirectory)),
            Err(fs_xfs::Error::NotFound) => {
                planned.insert(current.clone());
                missing.push(current.clone());
            }
            Err(e) => return Err(state_write_error(&current, e)),
        }
    }
    if !missing.is_empty() {
        if !fs.superblock().is_v5() {
            return Err(CliError::refused(format!(
                "{}: creating writes v5 metadata; a v4 filesystem is not supported",
                show(path)
            )));
        }
        if fs.was_replayed() {
            return Err(super::device::dirty_log(&target.to_string_lossy()));
        }
        drop(fs);
        let fs = super::device::mount_rw(target, offset)?;
        let created = (|| {
            for directory in &missing {
                let (parent_path, name) = split_path(directory)?;
                let parent = fs
                    .open_bytes(parent_path)
                    .map_err(|e| state_write_error(parent_path, e))?;
                fs.create_directory(parent.inode().ino, name, DIR_MODE)
                    .map_err(|e| state_write_error(directory, e))?;
            }
            Ok::<_, CliError>(())
        })();
        // Even a later allocation or I/O failure must release any earlier
        // creates clean, with both errors reported if release also fails.
        if let Err(error) = created {
            return Err(match release_clean(fs, path) {
                Ok(()) => error,
                Err(release) => {
                    CliError::failed(format!("{} (and then {})", error.message, release.message))
                }
            });
        }
        release_clean(fs, path)?;
    }
    let text = if missing.is_empty() {
        format!("already exists {}", show(path))
    } else {
        missing
            .iter()
            .map(|p| format!("created {}", show(p)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let report = Json::object([
        ("path", Json::from(show(path))),
        (
            "created",
            Json::Arr(missing.iter().map(|p| Json::from(show(p))).collect()),
        ),
    ]);
    Ok(Outcome::report(report).with_text(text))
}

/// A private, writable view of a read-only mount. Running the library's
/// truncate and clean release here checks its allocation, quota and log
/// limits too, without copying the image or opening it for writing.
struct TruncatePreview {
    source: Filesystem,
    writes: Mutex<Vec<(u64, Vec<u8>)>>,
}

impl BlockRead for TruncatePreview {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.source.device().read_at(offset, buf)?;
        for (at, bytes) in self.writes.lock().expect("preview poisoned").iter() {
            let start = offset.max(*at);
            let end = (offset + buf.len() as u64).min(*at + bytes.len() as u64);
            if start < end {
                buf[(start - offset) as usize..(end - offset) as usize]
                    .copy_from_slice(&bytes[(start - at) as usize..(end - at) as usize]);
            }
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        self.source.device().size_bytes()
    }
}

impl BlockDevice for TruncatePreview {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let size = self.size_bytes();
        if offset > size || buf.len() as u64 > size - offset {
            return Err(fs_core::Error::OutOfBounds {
                offset,
                len: buf.len() as u64,
                size,
            });
        }
        self.writes
            .lock()
            .expect("preview poisoned")
            .push((offset, buf.to_vec()));
        Ok(())
    }

    fn is_writable(&self) -> bool {
        true
    }
}

/// Set the length through truncate_to: unlike the older truncate, it
/// frees blocks and supports sparse growth. Refusals precede writable open.
fn truncate(target: &OsString, offset: u64, path: &[u8], size: u64) -> Result<Outcome, CliError> {
    if size > i64::MAX as u64 {
        return Err(CliError::refused(
            "truncate: size exceeds the signed file-length limit",
        ));
    }
    let fs = super::device::mount(target, offset)?;
    if fs.was_replayed() {
        return Err(super::device::dirty_log(&target.to_string_lossy()));
    }
    let file = fs
        .open_bytes(path)
        .map_err(|e| state_write_error(path, e))?;
    if path.ends_with(b"/") && !file.is_dir() {
        return Err(state_error(path, fs_xfs::Error::NotADirectory));
    }
    let ino = file.inode().ino;
    let preview = Filesystem::mount_rw(Arc::new(TruncatePreview {
        source: fs,
        writes: Mutex::new(Vec::new()),
    }))
    .map_err(|e| state_write_error(path, e))?;
    preview
        .truncate_to(ino, size)
        .map_err(|e| state_write_error(path, e))?;
    release_clean(preview, path)?;

    let fs = super::device::mount_rw(target, offset)?;
    fs.truncate_to(ino, size)
        .map_err(|e| state_write_error(path, e))?;
    release_clean(fs, path)?;
    Ok(Outcome::report(Json::object([
        ("path", Json::from(show(path))),
        ("size", Json::from(size)),
    ]))
    .with_text(format!("{} is {size} bytes", show(path))))
}

/// What a removal takes away: `rm` a name that is not a directory,
/// `rmdir` an empty directory.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Removal {
    Name,
    Directory,
}

/// Remove one name, and release the volume clean.
///
/// Everything the removal could be refused for that a read can decide is
/// decided first, on a read-only mount, so such a refusal never opens the
/// image for writing and leaves it byte-identical. The removal itself is
/// the library's `unlink_file` or `remove_directory`, journalled, which
/// checks again and refuses the shapes it does not free (a B+tree fork,
/// for one) before it writes anything. `unmount` then writes the change
/// home and ends the log with an unmount record, so `get dirty` is false
/// and the next write need not wait for Linux to replay it.
fn remove(
    target: &OsString,
    offset: u64,
    path: &[u8],
    removal: Removal,
) -> Result<Outcome, CliError> {
    let (parent, name) = removable(
        &super::device::mount(target, offset)?,
        target,
        path,
        removal,
    )?;
    let fs = super::device::mount_rw(target, offset)?;
    match removal {
        Removal::Name => fs.unlink_file(parent, name),
        Removal::Directory => fs.remove_directory(parent, name),
    }
    .map_err(|e| state_write_error(path, e))?;
    release_clean(fs, path)?;
    let mut fields = named(path);
    fields.push(("removed", Json::from(true)));
    Ok(Outcome::report(Json::object(fields)).with_text(format!("removed {}", show(path))))
}
/// A report's `path`, with its exact bytes in `path_hex` when it is not
/// UTF-8.
fn named(path: &[u8]) -> Vec<(&'static str, Json)> {
    let mut fields = vec![("path", Json::from(show(path)))];
    if std::str::from_utf8(path).is_err() {
        fields.push((
            "path_hex",
            Json::from(path.iter().map(|b| format!("{b:02x}")).collect::<String>()),
        ));
    }
    fields
}

/// `path` as the directory part, its last name, and whether a `/`
/// followed that name. The root's last name is empty.
fn last_name(path: &[u8]) -> (&[u8], &[u8], bool) {
    let end = path.iter().rposition(|&b| b != b'/').map_or(0, |i| i + 1);
    let trimmed = &path[..end];
    let start = trimmed
        .iter()
        .rposition(|&b| b == b'/')
        .map_or(0, |i| i + 1);
    let (dir, name) = trimmed.split_at(start);
    (dir, name, end < path.len())
}

/// The directory `dir` names, which holds `path`'s last name; its errors
/// name `path`.
fn parent_dir<'f>(fs: &'f Filesystem, path: &[u8], dir: &[u8]) -> Result<File<'f>, CliError> {
    let parent = fs.open_bytes(dir).map_err(|e| state_write_error(path, e))?;
    if !parent.is_dir() {
        return Err(state_error(path, fs_xfs::Error::NotADirectory));
    }
    Ok(parent)
}

/// The inode of the directory holding `path`'s last name, and that name,
/// once each refusal a read can decide has been ruled out: a v4 volume, a
/// log holding records, and then what the kernel gives `unlink(2)` and
/// `rmdir(2)`, for the same reason (`tests/cli_rm_kernel.rs` holds the
/// two to the same answers): a path that does not resolve, a directory
/// given to `rm` or a name ending in `/` that is not one, anything but a
/// directory given to `rmdir`, a directory with entries, `.`, `..` and the
/// root.
///
/// No path component is followed as a symlink, so a symlink is what `rm`
/// removes and what `rmdir` refuses as not a directory.
fn removable<'p>(
    fs: &Filesystem,
    target: &OsString,
    path: &'p [u8],
    removal: Removal,
) -> Result<(u64, &'p [u8]), CliError> {
    if !fs.superblock().is_v5() {
        return Err(CliError::refused(format!(
            "{}: removing writes v5 metadata; a v4 filesystem is not supported",
            show(path)
        )));
    }
    if fs.was_replayed() {
        return Err(super::device::dirty_log(&target.to_string_lossy()));
    }
    let failed = |why: &str| CliError::failed(format!("{}: {why}", show(path)));
    if path.is_empty() {
        return Err(state_error(path, fs_xfs::Error::NotFound));
    }
    let (dir, name, trailing_slash) = last_name(path);
    if name.is_empty() {
        return Err(match removal {
            Removal::Name => failed("is a directory"),
            Removal::Directory => failed("the root directory cannot be removed"),
        });
    }
    let parent = parent_dir(fs, path, dir)?;
    match (removal, name) {
        (Removal::Name, b"." | b"..") => return Err(failed("is a directory")),
        (Removal::Directory, b".") => {
            return Err(failed("invalid argument: rmdir cannot remove `.`"))
        }
        (Removal::Directory, b"..") => {
            return Err(state_error(path, fs_xfs::Error::DirectoryNotEmpty))
        }
        _ => {}
    }
    let child = parent.open_child(name).map_err(|e| state_error(path, e))?;
    match removal {
        Removal::Name if child.is_dir() => return Err(failed("is a directory")),
        Removal::Name if trailing_slash => {
            return Err(state_error(path, fs_xfs::Error::NotADirectory))
        }
        Removal::Name => {}
        Removal::Directory if !child.is_dir() => {
            return Err(state_error(path, fs_xfs::Error::NotADirectory))
        }
        Removal::Directory => {
            if !child
                .entries()
                .map_err(|e| state_error(path, e))?
                .is_empty()
            {
                return Err(state_error(path, fs_xfs::Error::DirectoryNotEmpty));
            }
        }
    }
    Ok((parent.inode().ino, name))
}

/// The longest name a directory entry holds, in bytes.
const NAME_MAX: usize = 255;

/// What a move hands the library's `rename`: the directory inode and name
/// SOURCE is at, and the directory inode and name PATH gives it.
struct Move<'a> {
    from_dir: u64,
    from: &'a [u8],
    to_dir: u64,
    to: &'a [u8],
}

/// Give SOURCE the name PATH, and release the volume clean.
///
/// As for a removal, everything the move could be refused for that a read
/// can decide is decided first, on a read-only mount, so such a refusal
/// never opens the image for writing and leaves it byte-identical. The
/// move itself is the library's journalled `rename`, which replaces what
/// is at PATH as `rename(2)` does and refuses the shapes it does not
/// change (a replaced file whose extents are in a B+tree, for one) before
/// it writes anything. `unmount` then writes the change home and ends the
/// log with an unmount record, so `get dirty` is false afterwards.
fn mv(target: &OsString, offset: u64, source: &[u8], path: &[u8]) -> Result<Outcome, CliError> {
    let names = movable(&super::device::mount(target, offset)?, target, source, path)?;
    let fs = super::device::mount_rw(target, offset)?;
    fs.rename(names.from_dir, names.from, names.to_dir, names.to)
        .map_err(|e| state_write_error(source, e))?;
    release_clean(fs, path)?;
    let mut fields = named(path);
    fields.push(("moved", Json::from(true)));
    Ok(Outcome::report(Json::object(fields)).with_text(format!(
        "moved {} to {}",
        show(source),
        show(path)
    )))
}

/// The [`Move`] SOURCE to PATH is, once each refusal a read can decide
/// has been ruled out: a v4 volume, a log holding records, and then what
/// the kernel gives `rename(2)`, for the same reason and in the order it
/// finds them (`tests/cli_mv_kernel.rs` holds the two to the same
/// answers): a parent that does not resolve, the root, `.` or `..` at
/// either end, a SOURCE that is not there, a new name too long for an
/// entry, a name ending in `/` on something that is not a directory, a
/// directory moved beneath itself, a file over a directory, a directory
/// over anything else, and a directory over one with entries.
///
/// No path component is followed as a symlink, so a symlink is what moves
/// and what is replaced. SOURCE and PATH naming the same inode is left to
/// the library, which changes nothing, as `rename(2)` does.
fn movable<'a>(
    fs: &Filesystem,
    target: &OsString,
    source: &'a [u8],
    path: &'a [u8],
) -> Result<Move<'a>, CliError> {
    if !fs.superblock().is_v5() {
        return Err(CliError::refused(format!(
            "{}: renaming writes v5 metadata; a v4 filesystem is not supported",
            show(source)
        )));
    }
    if fs.was_replayed() {
        return Err(super::device::dirty_log(&target.to_string_lossy()));
    }
    for given in [source, path] {
        if given.is_empty() {
            return Err(state_error(given, fs_xfs::Error::NotFound));
        }
    }
    let (from_dir, from, from_slash) = last_name(source);
    let (to_dir, to, to_slash) = last_name(path);
    let from_parent = parent_dir(fs, source, from_dir)?;
    let to_parent = parent_dir(fs, path, to_dir)?;
    for (given, name) in [(source, from), (path, to)] {
        if matches!(name, b"" | b"." | b"..") {
            return Err(CliError::failed(format!(
                "{}: the root, `.` and `..` cannot be moved or replaced",
                show(given)
            )));
        }
    }
    let moved = from_parent
        .open_child(from)
        .map_err(|e| state_error(source, e))?;
    if to.len() > NAME_MAX {
        return Err(CliError::failed(format!(
            "{}: file name too long: a name is at most {NAME_MAX} bytes",
            show(path)
        )));
    }
    if !moved.is_dir() && (from_slash || to_slash) {
        let given = if from_slash { source } else { path };
        return Err(state_error(given, fs_xfs::Error::NotADirectory));
    }
    if moved.is_dir()
        && on_the_way(fs, to_dir, moved.inode().ino).map_err(|e| state_error(path, e))?
    {
        return Err(CliError::failed(format!(
            "{}: invalid argument: a directory cannot be moved beneath itself, to {}",
            show(source),
            show(path)
        )));
    }
    match to_parent.open_child(to) {
        Ok(existing) if existing.inode().ino == moved.inode().ino => {}
        Ok(existing) => match (moved.is_dir(), existing.is_dir()) {
            (false, true) => {
                return Err(CliError::failed(format!("{}: is a directory", show(path))))
            }
            (true, false) => return Err(state_error(path, fs_xfs::Error::NotADirectory)),
            (true, true) => {
                if !existing
                    .entries()
                    .map_err(|e| state_error(path, e))?
                    .is_empty()
                {
                    return Err(state_error(path, fs_xfs::Error::DirectoryNotEmpty));
                }
            }
            (false, false) => {}
        },
        Err(fs_xfs::Error::NotFound) => {}
        Err(e) => return Err(state_error(path, e)),
    }
    Ok(Move {
        from_dir: from_parent.inode().ino,
        from,
        to_dir: to_parent.inode().ino,
        to,
    })
}

/// Whether inode `ino` is the directory `dir` names or one on the way to
/// it from the root. `dir` has resolved already, through no symlink and no
/// `..`, so its components are exactly that way.
fn on_the_way(fs: &Filesystem, dir: &[u8], ino: u64) -> Result<bool, fs_xfs::Error> {
    let mut at = fs.root()?.inode().ino;
    for component in dir
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
    {
        if at == ino {
            return Ok(true);
        }
        at = fs.open_ino(at)?.open_child(component)?.inode().ino;
    }
    Ok(at == ino)
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

fn set(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let key = sub.get_one::<String>("key").expect("clap requires the key");
    match key.as_str() {
        "label" => {
            let value = sub
                .get_one::<String>("value")
                .expect("clap requires the value");
            if value.len() > fs_xfs::superblock::offsets::FNAME_LEN {
                return Err(CliError::failed(format!(
                    "set label: {} bytes, and an XFS label holds at most {}",
                    value.len(),
                    fs_xfs::superblock::offsets::FNAME_LEN
                )));
            }
            let fs = super::device::mount_rw(target, offset)?;
            fs.set_label(value).map_err(|e| write_error(b"label", e))?;
            Ok(
                Outcome::report(Json::object([("label", Json::from(value.as_str()))]))
                    .with_text(format!("label set to {value:?}")),
            )
        }
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
    fn unknown_inode_flag_bits_are_kept_as_hex() {
        assert_eq!(inode_flag_names(0, &["KNOWN"]), Json::Arr(vec![]));
        assert_eq!(
            inode_flag_names(0x8000_0000_0000_0001, &["KNOWN"]),
            Json::Arr(vec![Json::from("KNOWN"), Json::from("0x8000000000000000")])
        );
    }

    #[test]
    fn a_path_splits_into_its_parent_and_its_name() {
        let split = |p: &str| {
            split_path(p.as_bytes())
                .map(|(a, b)| (show(a), show(b)))
                .map_err(|e| e.code)
        };
        assert_eq!(split("/f"), Ok(("/".into(), "f".into())));
        assert_eq!(split("/d/e/f"), Ok(("/d/e".into(), "f".into())));
        assert_eq!(split("/d/e/"), Ok(("/d".into(), "e".into())));
        assert_eq!(split("f"), Ok(("/".into(), "f".into())));
        for bad in ["/", "", "/d/..", "/d/."] {
            assert_eq!(split(bad), Err(2), "{bad:?}");
        }
    }

    #[test]
    fn a_uuid_is_written_in_its_standard_groups() {
        let u = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55,
            0x66, 0x77,
        ];
        assert_eq!(uuid_text(&u), "01234567-89ab-cdef-0011-223344556677");
    }
}
