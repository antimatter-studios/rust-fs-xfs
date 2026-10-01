//! Opening a target: an image file or a device, read-only or writable,
//! optionally `--offset` bytes in (a partition inside a whole-disk image).

use std::ffi::OsString;
use std::sync::Arc;

use fs_core::cli::CliError;
use fs_core::{BlockDevice, BlockRead, FileDevice, OwnedRwSlice, OwnedSlice};
use fs_xfs::Filesystem;

/// The window `offset` bytes into a device of `size` bytes, or the reason
/// there is none.
fn window(name: &str, offset: u64, size: u64) -> Result<u64, CliError> {
    if offset >= size {
        return Err(CliError::failed(format!(
            "--offset {offset} is past the end of {name} ({size} bytes)"
        )));
    }
    Ok(size - offset)
}

fn open_file(name: &str, writable: bool) -> Result<FileDevice, CliError> {
    if writable {
        FileDevice::open_rw(name)
    } else {
        FileDevice::open(name)
    }
    .map_err(|e| {
        CliError::failed(format!(
            "open {name}{}: {e}",
            if writable { " read-write" } else { "" }
        ))
    })
}

/// Mount `target` read-only, `offset` bytes in.
///
/// A volume whose log holds records nothing has applied is replayed into
/// memory, as the library's read-only mount always does: what is reported
/// is the filesystem the kernel would recover, and the image is not
/// touched. `get dirty` says when that happened.
pub fn mount(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    let name = target.to_string_lossy();
    let dev: Arc<dyn BlockRead> = Arc::new(open_file(&name, false)?);
    let dev: Arc<dyn BlockRead> = if offset == 0 {
        dev
    } else {
        let length = window(&name, offset, dev.size_bytes())?;
        Arc::new(OwnedSlice::new(dev, offset, length))
    };
    Filesystem::mount(dev).map_err(|e| not_readable(&name, e))
}

/// Mount `target` read-write, `offset` bytes in.
pub fn mount_rw(target: &OsString, offset: u64) -> Result<Filesystem, CliError> {
    let name = target.to_string_lossy();
    let dev: Arc<dyn BlockDevice> = Arc::new(open_file(&name, true)?);
    let dev: Arc<dyn BlockDevice> = if offset == 0 {
        dev
    } else {
        let length = window(&name, offset, dev.size_bytes())?;
        Arc::new(OwnedRwSlice::new(dev, offset, length))
    };
    Filesystem::mount_rw(dev).map_err(|e| match e {
        fs_xfs::Error::DirtyLog => CliError::failed(format!(
            "{name}: the log holds records nothing has applied, and this driver writes \
             only to a volume whose log is clean. Mount it with Linux once (which replays \
             the log) and try again; reading it works as it is"
        )),
        fs_xfs::Error::UnsupportedFeature(_) | fs_xfs::Error::ReadOnly => {
            CliError::refused(format!("{name}: {e}"))
        }
        other => not_readable(&name, other),
    })
}

fn not_readable(name: &str, e: fs_xfs::Error) -> CliError {
    CliError::failed(format!("{name} is not a readable XFS filesystem: {e}"))
}
