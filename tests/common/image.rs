//! Reading disk images in bounded memory (#606).
//!
//! An image here is hundreds of megabytes, and libtest runs one test per CPU,
//! so a test that holds one or two copies of an image in memory multiplies that
//! by the number of tests running. Everything below holds at most a few
//! mebibytes however large the image is: a range read, a streamed digest, a
//! sparse copy on disk, and a comparison of two files one chunk at a time.
//!
//! `tests/scripts/no-whole-image-reads.sh` refuses a whole-file read anywhere
//! else under `tests/`.

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

const CHUNK: usize = 1 << 20;

/// The largest file [`small_file`] reads whole.
pub const SMALL_FILE_LIMIT: u64 = 1 << 20;

/// The start of an image that holds its primary superblock, whatever the
/// sector size: XFS sectors are at most 32 KiB.
const HEAD: u64 = 64 << 10;

/// An image read a range at a time.
pub struct Image {
    file: File,
    len: u64,
}

impl Image {
    pub fn open(path: impl AsRef<Path>) -> Image {
        let path = path.as_ref();
        let file =
            File::open(path).unwrap_or_else(|e| panic!("open the image {}: {e}", path.display()));
        let len = file
            .metadata()
            .unwrap_or_else(|e| panic!("stat the image {}: {e}", path.display()))
            .len();
        Image { file, len }
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    /// Fill `buf` from `offset`. The whole range must be inside the image.
    pub fn read_into(&self, offset: u64, buf: &mut [u8]) {
        self.file
            .read_exact_at(buf, offset)
            .unwrap_or_else(|e| panic!("read {} bytes at {offset}: {e}", buf.len()));
    }

    /// The `len` bytes at `offset`.
    pub fn at(&self, offset: u64, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        self.read_into(offset, &mut buf);
        buf
    }

    /// The start of the image, where the primary superblock is.
    pub fn head(&self) -> Vec<u8> {
        self.at(0, HEAD.min(self.len) as usize)
    }
}

/// The start of the image at `path`, where its primary superblock is.
pub fn head(path: impl AsRef<Path>) -> Vec<u8> {
    Image::open(path).head()
}

/// A file small by construction, read whole: a literal payload, a corpus seed.
/// A file over [`SMALL_FILE_LIMIT`] is refused, so an image cannot arrive here.
pub fn small_file(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(SMALL_FILE_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > SMALL_FILE_LIMIT {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("the file is over the {SMALL_FILE_LIMIT} byte whole-read limit"),
        ));
    }
    Ok(bytes)
}

/// The SHA-256 of a file, in lower-case hex, read one chunk at a time.
pub fn sha256_file(path: impl AsRef<Path>) -> String {
    let path = path.as_ref();
    let mut file = File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let mut digest = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = file
            .read(&mut buf)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Where two files first differ.
#[derive(Debug, PartialEq, Eq)]
pub enum Difference {
    Length { left: u64, right: u64 },
    Byte { offset: u64, left: u8, right: u8 },
}

impl fmt::Display for Difference {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Difference::Length { left, right } => {
                write!(f, "the lengths differ: {left} and {right} bytes")
            }
            Difference::Byte {
                offset,
                left,
                right,
            } => write!(
                f,
                "the first differing byte is at {offset} ({offset:#x}): {left:#04x} and {right:#04x}"
            ),
        }
    }
}

/// The first difference between two files outside `ignored`, or `None`.
pub fn first_difference_outside(
    left: impl AsRef<Path>,
    right: impl AsRef<Path>,
    ignored: Range<u64>,
) -> Option<Difference> {
    let left = Image::open(left);
    let right = Image::open(right);
    if left.len != right.len {
        return Some(Difference::Length {
            left: left.len,
            right: right.len,
        });
    }
    let mut a = vec![0u8; CHUNK];
    let mut b = vec![0u8; CHUNK];
    let mut at = 0u64;
    while at < left.len {
        let n = (left.len - at).min(CHUNK as u64) as usize;
        left.read_into(at, &mut a[..n]);
        right.read_into(at, &mut b[..n]);
        if a[..n] != b[..n] {
            let differing = (0..n).find(|&i| a[i] != b[i] && !ignored.contains(&(at + i as u64)));
            if let Some(i) = differing {
                return Some(Difference::Byte {
                    offset: at + i as u64,
                    left: a[i],
                    right: b[i],
                });
            }
        }
        at += n as u64;
    }
    None
}

/// The first difference between two files, or `None`.
pub fn first_difference(left: impl AsRef<Path>, right: impl AsRef<Path>) -> Option<Difference> {
    first_difference_outside(left, right, 0..0)
}

/// A copy of an image as it was, kept on disk and removed when dropped.
pub struct Snapshot(PathBuf);

impl Snapshot {
    /// Copy `image` sparsely: a chunk of zeros is a hole in the copy.
    pub fn of(image: impl AsRef<Path>) -> Snapshot {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let image = image.as_ref();
        let path = std::env::temp_dir().join(format!(
            "image-snapshot-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let snapshot = Snapshot(path);
        copy_sparse(image, &snapshot.0)
            .unwrap_or_else(|e| panic!("snapshot {}: {e}", image.display()));
        snapshot
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Fail, naming the first differing byte, unless `image` still holds
    /// exactly what it held when this was taken.
    #[track_caller]
    pub fn assert_unchanged(&self, image: impl AsRef<Path>, context: &str) {
        if let Some(difference) = first_difference(&self.0, image) {
            panic!("{context}: {difference}");
        }
    }
}

impl Drop for Snapshot {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn copy_sparse(from: &Path, to: &Path) -> io::Result<()> {
    let mut source = File::open(from)?;
    let len = source.metadata()?.len();
    let mut copy = File::create(to)?;
    let mut buf = vec![0u8; CHUNK];
    let mut at = 0u64;
    while at < len {
        let n = (len - at).min(CHUNK as u64) as usize;
        source.read_exact(&mut buf[..n])?;
        if buf[..n].iter().any(|&b| b != 0) {
            copy.seek(SeekFrom::Start(at))?;
            copy.write_all(&buf[..n])?;
        }
        at += n as u64;
    }
    copy.set_len(len)
}
