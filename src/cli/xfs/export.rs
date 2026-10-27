//! `export`: copy a file or a directory tree out of the image through
//! rust-fs-core's shared transfer. This file only says what the library
//! can tell the transfer about the image -- a lookup, a listing, a stat
//! (identity, type, size, times, symlink target), the extended attributes
//! and a positioned read -- so the bounded chunks, the staged publication, the refusal to replace
//! and the per-item report are the shared ones, not a copy loop of ours.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::ArgMatches;
use fs_core::cli::transfer::{
    export_args_with_xattrs, export_tree_with_sparse, export_tree_with_xattrs_and_progress,
    export_with_sparse, ExportOptions, ExportRequest, ExtendedAttribute, FileSource, LinkOptions,
    NamedStream, Progress, ProgressOptions, SparseOptions, SymbolicLink, TimeOptions, TreeEntry,
    TreeKind, TreeMetadata, TreeOptions, TreeSource, XattrOptions,
};
use fs_core::cli::{CliError, Outcome};
use fs_xfs::fs::File;
use fs_xfs::Filesystem;

use super::fs::{read_error, show, xfs_error};

/// The arguments `export` takes: the shared set, so every driver's parse
/// alike.
pub fn args() -> [clap::Arg; 7] {
    export_args_with_xattrs()
}

#[cfg(unix)]
pub(super) fn bytes_of(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
pub(super) fn bytes_of(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn path_of(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

/// The names an ACL is stored under. `list_xattrs` also gives each one
/// under its `system.posix_acl_*` name, in the format a host sets; only that
/// view leaves the image, or the host would get the ACL twice.
const STORED_ACLS: [&[u8]; 2] = [b"trusted.SGI_ACL_FILE", b"trusted.SGI_ACL_DEFAULT"];

fn time(t: fs_xfs::inode::Timestamp) -> Option<SystemTime> {
    let nanos = Duration::from_nanos(u64::from(t.nsec));
    if t.sec >= 0 {
        UNIX_EPOCH.checked_add(Duration::from_secs(t.sec.unsigned_abs()) + nanos)
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(t.sec.unsigned_abs()))?
            .checked_add(nanos)
    }
}

fn metadata_of(file: &File<'_>) -> TreeMetadata {
    let inode = file.inode();
    let kind = if file.is_regular_file() {
        TreeKind::File
    } else if file.is_dir() {
        TreeKind::Directory
    } else {
        TreeKind::Other
    };
    TreeMetadata {
        kind,
        size: file.len(),
        inode: inode.ino,
        accessed: time(inode.atime),
        modified: time(inode.mtime),
    }
}

/// The image as the transfer reads it. Names are not followed: a symbolic
/// link is `Other`, with its stored target given on request.
struct Source<'f> {
    fs: &'f Filesystem,
    image: PathBuf,
}

impl Source<'_> {
    fn open(&self, path: &Path) -> Result<File<'_>, CliError> {
        let bytes = bytes_of(path);
        self.fs
            .open_bytes(&bytes)
            .map_err(|e| read_error(&bytes, e))
    }
}

impl TreeSource for Source<'_> {
    fn image_path(&self) -> Option<&Path> {
        Some(&self.image)
    }

    fn symbolic_link(&mut self, path: &Path) -> Result<Option<SymbolicLink>, CliError> {
        let file = self.open(path)?;
        if !file.is_symlink() {
            return Ok(None);
        }
        let target = file
            .link_target()
            .map_err(|e| xfs_error(&bytes_of(path), e))?;
        Ok(Some(SymbolicLink {
            target: path_of(target),
            target_is_directory: false,
        }))
    }

    fn metadata(&mut self, path: &Path) -> Result<TreeMetadata, CliError> {
        Ok(metadata_of(&self.open(path)?))
    }

    fn read_dir(&mut self, path: &Path) -> Result<Vec<TreeEntry>, CliError> {
        let bytes = bytes_of(path);
        let dir = self.open(path)?;
        let mut children = dir.entries().map_err(|e| xfs_error(&bytes, e))?;
        children.retain(|d| d.name != b"." && d.name != b"..");
        children.sort_by(|a, b| a.name.cmp(&b.name));
        children
            .into_iter()
            .map(|d| {
                let mut full = bytes.clone();
                if !full.ends_with(b"/") {
                    full.push(b'/');
                }
                full.extend_from_slice(&d.name);
                let file = self.fs.open_ino(d.ino).map_err(|e| xfs_error(&full, e))?;
                Ok(TreeEntry {
                    name: path_of(d.name).into_os_string(),
                    metadata: metadata_of(&file),
                })
            })
            .collect()
    }

    fn read_file_at(
        &mut self,
        path: &Path,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, CliError> {
        self.open(path)?
            .read_at(offset, buf)
            .map_err(|e| xfs_error(&bytes_of(path), e))
    }

    fn extended_attributes(&mut self, path: &Path) -> Result<Vec<ExtendedAttribute>, CliError> {
        let file = self.open(path)?;
        let attrs = self
            .fs
            .list_xattrs(file.inode(), file.raw())
            .map_err(|e| xfs_error(&bytes_of(path), e))?;
        Ok(attrs
            .into_iter()
            .filter(|a| !STORED_ACLS.contains(&a.name.as_slice()))
            .map(|a| ExtendedAttribute {
                name: path_of(a.name).into_os_string(),
                value: a.value,
            })
            .collect())
    }

    /// XFS has no named streams: an attribute is never one.
    fn named_streams(&mut self, _path: &Path) -> Result<Vec<NamedStream>, CliError> {
        Ok(Vec::new())
    }
}

/// One regular file, opened once: its length and a positioned read.
struct One<'f> {
    file: File<'f>,
    path: Vec<u8>,
    image: PathBuf,
}

impl FileSource for One<'_> {
    fn image_path(&self) -> Option<&Path> {
        Some(&self.image)
    }

    fn size(&self) -> u64 {
        self.file.len()
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> Result<usize, CliError> {
        self.file
            .read_at(offset, buf)
            .map_err(|e| xfs_error(&self.path, e))
    }
}

/// `export [--recursive] PATH DEST`: a file goes to DEST (or to stdout
/// for `-`), a tree to a directory DEST, every item reported.
pub fn export(fs: &Filesystem, image: &OsString, sub: &ArgMatches) -> Result<Outcome, CliError> {
    let request = ExportRequest::from_matches(sub)?;
    let tree = TreeOptions::from_matches(sub)?;
    let links = LinkOptions::from_matches(sub)?;
    let times = TimeOptions::from_matches(sub)?;
    let sparse = SparseOptions::from_matches(sub)?;
    let xattrs = XattrOptions::from_matches(sub)?;
    xattrs_usable(xattrs, tree.recursive, sparse)?;
    let image = PathBuf::from(image);
    let path = bytes_of(Path::new(&request.path));
    let file = fs.open_bytes(&path).map_err(|e| read_error(&path, e))?;
    if tree.recursive {
        let mut stderr = std::io::stderr();
        let terminal = std::io::IsTerminal::is_terminal(&stderr);
        let mut progress = Progress::new(&mut stderr, terminal, ProgressOptions::default());
        if xattrs.preserve_xattrs {
            return export_tree_with_xattrs_and_progress(
                &request,
                &mut Source { fs, image },
                tree,
                links,
                times,
                xattrs,
                &mut progress,
            );
        }
        return export_tree_with_sparse(
            &request,
            &mut Source { fs, image },
            tree,
            links,
            times,
            sparse,
            &mut progress,
        );
    }
    if file.is_dir() {
        return Err(CliError::failed(format!(
            "{}: is a directory; export it with --recursive",
            show(&path)
        )));
    }
    if file.is_symlink() {
        let target = file.link_target().map(|t| show(&t)).unwrap_or_default();
        return Err(CliError::failed(format!(
            "{}: is a symlink to {target}; export the target instead, or the tree with \
             --recursive --preserve=links",
            show(&path)
        )));
    }
    if !file.is_regular_file() {
        return Err(CliError::failed(format!(
            "{}: not a regular file",
            show(&path)
        )));
    }
    export_with_sparse(
        &request,
        &mut One { file, path, image },
        ExportOptions::from_matches(sub)?,
        sparse,
    )
}

/// Refuse `--preserve=xattrs` where the shared transfer has no way to carry
/// it, rather than copy the item and drop its attributes: the attribute
/// transfer is a tree transfer, and it does not also keep holes.
pub(super) fn xattrs_usable(
    xattrs: XattrOptions,
    recursive: bool,
    sparse: SparseOptions,
) -> Result<(), CliError> {
    if !xattrs.preserve_xattrs {
        return Ok(());
    }
    if !recursive {
        return Err(CliError::usage(
            "--preserve=xattrs carries attributes through a tree transfer; add --recursive",
        ));
    }
    if sparse.preserve_sparse {
        return Err(CliError::usage(
            "--preserve=xattrs and --preserve=sparse cannot be combined in one transfer yet",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xattrs(on: bool) -> XattrOptions {
        XattrOptions {
            preserve_xattrs: on,
        }
    }

    fn sparse(on: bool) -> SparseOptions {
        SparseOptions {
            preserve_sparse: on,
        }
    }

    #[test]
    fn attributes_need_a_tree_and_no_holes() {
        assert!(xattrs_usable(xattrs(false), false, sparse(true)).is_ok());
        assert!(xattrs_usable(xattrs(true), true, sparse(false)).is_ok());
        let one_file = xattrs_usable(xattrs(true), false, sparse(false)).unwrap_err();
        assert!(
            one_file.message.contains("--recursive"),
            "{}",
            one_file.message
        );
        let holes = xattrs_usable(xattrs(true), true, sparse(true)).unwrap_err();
        assert!(holes.message.contains("sparse"), "{}", holes.message);
    }
}
