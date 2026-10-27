//! `import`: stream a local file, stdin or a local tree into the image
//! through rust-fs-core's shared import, publishing a file only when it is
//! complete. This file is the shared sink over this library's write API:
//!
//! - **stage**: a file with a private name is created beside the
//!   destination, in the destination's directory;
//! - **write**: bytes go in at increasing offsets, a bounded chunk at a
//!   time, through the library's journalled positioned write, which
//!   allocates and grows the file;
//! - **persist**: the volume is released clean (the log closed with an
//!   unmount record), so what was written is on the device;
//! - **read back**: for `--verify`, through a fresh read-only open of the
//!   image, never through the mount the writes went to;
//! - **publish**: the staged name is renamed to the final name. The rename
//!   is one journalled record, and it replaces an existing name in that
//!   same record, which is why `--replace` is offered. The old file is
//!   never deleted first;
//! - **abort**: the staged name is unlinked. The final name is not touched.
//!
//! `--preserve=links,times` use the sink's optional capabilities: a symbolic
//! link is created with `create_symlink`, another name for a published file
//! with `link`, and times are set in place with `set_attributes`. Each is
//! one operation on a clean volume that is released clean again.
//!
//! A crash between persist and publish leaves the old file (or no file)
//! under the final name and the staged name, `.fs-xfs-import-PID-N.partial`,
//! beside it: the image is consistent, and the leftover is an ordinary file
//! that `rm` removes.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgMatches};
use fs_core::cli::transfer::{
    import_args_with_xattrs, import_tree_with_sparse, import_tree_with_xattrs, import_with_sparse,
    ExtendedAttribute, FileSink, ImportOptions, ImportRequest, ImportTreeOptions, SparseOptions,
    TreeSink, XattrOptions,
};
use fs_core::cli::{CliError, Outcome};
use fs_xfs::attr_write::XattrMode;
use fs_xfs::inode::Timestamp;
use fs_xfs::write::AttrChange;
use fs_xfs::Filesystem;
use std::time::{SystemTime, UNIX_EPOCH};

use super::export::{bytes_of, xattrs_usable};
use super::fs::{
    release_clean, show, split_path, state_error, state_write_error, write_error, xfs_error,
    DIR_MODE, FILE_MODE,
};

/// The arguments `import` takes: the shared tree set, and `--verify`.
pub fn args() -> [Arg; 7] {
    let [source, path, replace, recursive, depth, preserve] = import_args_with_xattrs();
    [
        source,
        path,
        replace,
        recursive,
        depth,
        preserve,
        Arg::new("verify")
            .long("verify")
            .action(ArgAction::SetTrue)
            .help(
                "Read the persisted staged file back and verify its length and SHA-256 \
                 before publishing (one file, not --recursive)",
            ),
    ]
}

/// The file being staged: where it is, and what it will be called.
struct Staged {
    /// The directory both names are in.
    dir: u64,
    /// The private name the bytes go to.
    stage: Vec<u8>,
    /// The name it is published under.
    name: Vec<u8>,
    /// The staged file's inode.
    ino: u64,
    /// The destination as given, for messages.
    path: Vec<u8>,
}

/// How much is gathered before it is written. The library's write maps
/// each call's blocks as extents of their own and a file's extent list is
/// held in its inode (21 at 512-byte inodes), so one extent per 1 MiB chunk
/// would refuse a file past 21 MiB. Gathering keeps the memory held bounded,
/// and the extents few.
const GATHER: usize = 32 << 20;

struct Sink {
    target: OsString,
    offset: u64,
    image: PathBuf,
    /// The writable mount, open from `stage` to `persist`.
    rw: Option<Filesystem>,
    /// A fresh read-only mount, for reading the staged file back.
    ro: Option<Filesystem>,
    staged: Option<Staged>,
    counter: u32,
    /// Bytes accepted by `write_at` and not yet written, and where they go.
    gathered: Vec<u8>,
    gathered_at: u64,
}

impl Sink {
    fn new(target: &OsString, offset: u64) -> Sink {
        Sink {
            target: target.clone(),
            offset,
            image: PathBuf::from(target),
            rw: None,
            ro: None,
            staged: None,
            counter: 0,
            gathered: Vec::new(),
            gathered_at: 0,
        }
    }

    /// Write what has been gathered, in one call to the library.
    fn flush(&mut self) -> Result<(), CliError> {
        if self.gathered.is_empty() {
            return Ok(());
        }
        let (Some(fs), Some(staged)) = (self.rw.as_ref(), self.staged.as_ref()) else {
            return Err(CliError::failed("write before stage"));
        };
        let written = fs
            .write(staged.ino, self.gathered_at, &self.gathered)
            .map(|_| ())
            .map_err(|e| write_error(&staged.path, e));
        self.gathered.clear();
        written
    }

    fn mount_rw(&mut self) -> Result<Filesystem, CliError> {
        match self.rw.take() {
            Some(fs) => Ok(fs),
            None => super::device::mount_rw(&self.target, self.offset),
        }
    }

    /// Release the writable mount clean, if one is open.
    fn release(&mut self, what: &[u8]) -> Result<(), CliError> {
        match self.rw.take() {
            Some(fs) => release_clean(fs, what),
            None => Ok(()),
        }
    }
}

/// A name the library can store: not empty, no `/` or NUL, at most 255 bytes.
fn checked_name(name: &[u8]) -> Result<(), CliError> {
    if name.is_empty() || name == b"." || name == b".." {
        return Err(CliError::refused(format!(
            "{}: not a usable name",
            show(name)
        )));
    }
    if name.len() > 255 {
        return Err(CliError::refused(format!(
            "{}: {} bytes, and XFS names hold at most 255",
            show(name),
            name.len()
        )));
    }
    if name.contains(&0) || name.contains(&b'/') {
        return Err(CliError::refused(format!(
            "{}: XFS names hold no NUL and no /",
            show(name)
        )));
    }
    Ok(())
}

impl FileSink for Sink {
    fn image_path(&self) -> Option<&Path> {
        Some(&self.image)
    }

    fn supports_atomic_replace(&self) -> bool {
        true
    }

    fn stage(&mut self, path: &Path, replace: bool) -> Result<(), CliError> {
        let what = bytes_of(path);
        let (parent_path, name) = split_path(&what)?;
        checked_name(name)?;
        self.ro = None;
        let fs = self.mount_rw()?;
        let parent = fs
            .open_bytes(parent_path)
            .map_err(|e| state_error(parent_path, e))?;
        if !parent.is_dir() {
            return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
        }
        match parent.open_child(name) {
            Ok(existing) if !replace => {
                let _ = existing;
                return Err(CliError::failed(format!(
                    "{}: exists; give --replace to replace it",
                    show(&what)
                )));
            }
            Ok(existing) if !existing.is_regular_file() => {
                return Err(CliError::refused(format!(
                    "{}: exists and is not a regular file; --replace replaces files only",
                    show(&what)
                )));
            }
            Ok(_) | Err(fs_xfs::Error::NotFound) => {}
            Err(e) => return Err(xfs_error(&what, e)),
        }
        let dir = parent.inode().ino;
        self.counter += 1;
        let stage = format!(
            ".fs-xfs-import-{}-{}.partial",
            std::process::id(),
            self.counter
        )
        .into_bytes();
        let (ino, _) = fs
            .create_file(dir, &stage, FILE_MODE)
            .map_err(|e| write_error(&what, e))?;
        self.staged = Some(Staged {
            dir,
            stage,
            name: name.to_vec(),
            ino,
            path: what,
        });
        self.rw = Some(fs);
        Ok(())
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), CliError> {
        if self.rw.is_none() || self.staged.is_none() {
            return Err(CliError::failed("write_at before stage"));
        }
        if !self.gathered.is_empty() && self.gathered_at + self.gathered.len() as u64 != offset {
            self.flush()?;
        }
        if self.gathered.is_empty() {
            self.gathered_at = offset;
        }
        self.gathered.extend_from_slice(bytes);
        if self.gathered.len() >= GATHER {
            self.flush()?;
        }
        Ok(())
    }

    fn persist(&mut self) -> Result<(), CliError> {
        self.flush()?;
        let what = self
            .staged
            .as_ref()
            .map(|s| s.path.clone())
            .unwrap_or_default();
        self.release(&what)
    }

    fn read_at(&mut self, offset: u64, bytes: &mut [u8]) -> Result<usize, CliError> {
        let Some(staged) = self.staged.as_ref() else {
            return Err(CliError::failed("read_at before stage"));
        };
        if self.ro.is_none() {
            self.ro = Some(super::device::mount(&self.target, self.offset)?);
        }
        let fs = self.ro.as_ref().expect("mounted above");
        let file = fs
            .open_ino(staged.ino)
            .map_err(|e| xfs_error(&staged.path, e))?;
        if offset >= file.len() {
            return Ok(0);
        }
        file.read_at(offset, bytes)
            .map_err(|e| xfs_error(&staged.path, e))
    }

    fn publish(&mut self, replace: bool) -> Result<bool, CliError> {
        self.ro = None;
        let Some(staged) = self.staged.as_ref() else {
            return Err(CliError::failed("publish before stage"));
        };
        let (dir, stage, name, what) = (
            staged.dir,
            staged.stage.clone(),
            staged.name.clone(),
            staged.path.clone(),
        );
        let fs = self.mount_rw()?;
        let existing = match fs
            .open_ino(dir)
            .map_err(|e| xfs_error(&what, e))?
            .open_child(&name)
        {
            Ok(file) => Some(file.is_regular_file()),
            Err(fs_xfs::Error::NotFound) => None,
            Err(e) => return Err(xfs_error(&what, e)),
        };
        match existing {
            Some(_) if !replace => {
                self.rw = Some(fs);
                return Err(CliError::failed(format!(
                    "{}: appeared since it was staged; give --replace to replace it",
                    show(&what)
                )));
            }
            Some(false) => {
                self.rw = Some(fs);
                return Err(CliError::refused(format!(
                    "{}: exists and is not a regular file",
                    show(&what)
                )));
            }
            _ => {}
        }
        if let Err(e) = fs.rename(dir, &stage, dir, &name) {
            self.rw = Some(fs);
            return Err(state_write_error(&what, e));
        }
        self.staged = None;
        release_clean(fs, &what)?;
        Ok(existing.is_some())
    }

    fn abort(&mut self) -> Result<(), CliError> {
        self.ro = None;
        self.gathered = Vec::new();
        let Some(staged) = self.staged.take() else {
            return self.release(b"");
        };
        let fs = self.mount_rw().map_err(|e| {
            CliError::failed(format!(
                "{}: the staged file {} could not be removed: {}",
                show(&staged.path),
                show(&staged.stage),
                e.message
            ))
        })?;
        match fs.unlink_file(staged.dir, &staged.stage) {
            Ok(_) | Err(fs_xfs::Error::NotFound) => {}
            Err(e) => {
                self.rw = Some(fs);
                return Err(CliError::failed(format!(
                    "{}: the staged file {} could not be removed: {e}",
                    show(&staged.path),
                    show(&staged.stage)
                )));
            }
        }
        release_clean(fs, &staged.path)
    }
}

impl TreeSink for Sink {
    fn validate_name(&mut self, _parent: &Path, name: &OsStr) -> Result<OsString, CliError> {
        let bytes = bytes_of(Path::new(name));
        checked_name(&bytes)?;
        Ok(name.to_os_string())
    }

    fn create_dir(&mut self, path: &Path) -> Result<bool, CliError> {
        let what = bytes_of(path);
        let (parent_path, name) = split_path(&what)?;
        checked_name(name)?;
        let fs = self.mount_rw()?;
        let parent = fs
            .open_bytes(parent_path)
            .map_err(|e| state_error(parent_path, e))?;
        if !parent.is_dir() {
            return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
        }
        match parent.open_child(name) {
            Ok(existing) if existing.is_dir() => return Ok(true),
            Ok(_) => {
                return Err(CliError::refused(format!(
                    "{}: exists and is not a directory",
                    show(&what)
                )))
            }
            Err(fs_xfs::Error::NotFound) => {}
            Err(e) => return Err(xfs_error(&what, e)),
        }
        fs.create_directory(parent.inode().ino, name, DIR_MODE)
            .map_err(|e| state_write_error(&what, e))?;
        release_clean(fs, &what)?;
        Ok(false)
    }

    fn format_name(&self) -> &str {
        "XFS"
    }

    fn create_symbolic_link(&mut self, path: &Path, target: &Path) -> Result<(), CliError> {
        let what = bytes_of(path);
        let (parent_path, name) = split_path(&what)?;
        checked_name(name)?;
        let fs = self.mount_rw()?;
        let parent = fs
            .open_bytes(parent_path)
            .map_err(|e| state_error(parent_path, e))?;
        if !parent.is_dir() {
            return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
        }
        fs.create_symlink(parent.inode().ino, name, &bytes_of(target))
            .map_err(|e| state_write_error(&what, e))?;
        release_clean(fs, &what)
    }

    fn create_hard_link(&mut self, path: &Path, existing: &Path) -> Result<(), CliError> {
        let what = bytes_of(path);
        let (parent_path, name) = split_path(&what)?;
        checked_name(name)?;
        let fs = self.mount_rw()?;
        let other = bytes_of(existing);
        let file = fs.open_bytes(&other).map_err(|e| state_error(&other, e))?;
        if !file.is_regular_file() {
            return Err(CliError::refused(format!(
                "{}: hard links to regular files only",
                show(&other)
            )));
        }
        let parent = fs
            .open_bytes(parent_path)
            .map_err(|e| state_error(parent_path, e))?;
        if !parent.is_dir() {
            return Err(state_error(parent_path, fs_xfs::Error::NotADirectory));
        }
        fs.link(file.inode().ino, parent.inode().ino, name)
            .map_err(|e| state_write_error(&what, e))?;
        release_clean(fs, &what)
    }

    fn set_times(
        &mut self,
        path: &Path,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), CliError> {
        let what = bytes_of(path);
        let change = AttrChange {
            atime: accessed.map(timestamp),
            mtime: modified.map(timestamp),
            ..AttrChange::default()
        };
        let fs = self.mount_rw()?;
        let file = fs.open_bytes(&what).map_err(|e| state_error(&what, e))?;
        fs.set_attributes(file.inode(), &change)
            .map_err(|e| write_error(&what, e))?;
        release_clean(fs, &what)
    }

    /// Set one host attribute under the name the host gave it. A name
    /// outside the user, trusted and security namespaces, or an ACL, is
    /// refused by the library, and so is the item, never the attribute
    /// dropped.
    fn set_extended_attribute(
        &mut self,
        path: &Path,
        attribute: &ExtendedAttribute,
    ) -> Result<(), CliError> {
        let what = bytes_of(path);
        let name = bytes_of(Path::new(&attribute.name));
        let fs = self.mount_rw()?;
        let file = fs.open_bytes(&what).map_err(|e| state_error(&what, e))?;
        fs.set_xattr(file.inode().ino, &name, &attribute.value, XattrMode::Set)
            .map_err(|e| write_error(&what, e))?;
        release_clean(fs, &what)
    }
}

/// A time as the format stores it: seconds from the epoch, negative before
/// it, and the nanoseconds past those seconds.
fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Timestamp {
            sec: after.as_secs() as i64,
            nsec: after.subsec_nanos(),
        },
        Err(before) => {
            let before = before.duration();
            let nsec = before.subsec_nanos();
            Timestamp {
                sec: -(before.as_secs() as i64) - i64::from(nsec > 0),
                nsec: if nsec > 0 { 1_000_000_000 - nsec } else { 0 },
            }
        }
    }
}

/// `import [--recursive] [--replace] [--verify] SOURCE PATH`.
pub fn import(target: &OsString, offset: u64, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let request = ImportRequest::from_matches(sub)?;
    let tree = ImportTreeOptions::from_matches(sub)?;
    let options = ImportOptions::from_matches(sub)?;
    let sparse = SparseOptions::from_matches(sub)?;
    let xattrs = XattrOptions::from_matches(sub)?;
    xattrs_usable(xattrs, tree.recursive, sparse)?;
    let mut sink = Sink::new(target, offset);
    if tree.recursive {
        if options.verify {
            return Err(CliError::usage(
                "--verify reads one staged file back; it does not apply to --recursive",
            ));
        }
        if xattrs.preserve_xattrs {
            return import_tree_with_xattrs(&request, Some(&mut sink), tree, xattrs);
        }
        return import_tree_with_sparse(&request, Some(&mut sink), tree, sparse);
    }
    import_with_sparse(&request, options, sparse, Some(&mut sink))
}
