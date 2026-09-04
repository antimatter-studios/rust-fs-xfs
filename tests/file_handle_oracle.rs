//! The `File` handle, checked against a filesystem `mkfs.xfs` built.
//!
//! Two things are under test and they are different in kind.
//!
//! **That it reads the right bytes.** Every `File` method is compared
//! against the low-level call it wraps, on the same image. If they ever
//! disagree, one of them is wrong and the handle is not a thin wrapper.
//!
//! **That it removes a read.** `Filesystem::open` keeps the raw inode
//! fork its path walk already fetched, where `lookup_path` discarded it
//! and left the caller to fetch it again. That is the reason the type
//! exists, so it is measured rather than asserted: the device is wrapped
//! in a counter and the two routes are compared. A claim about I/O that
//! nothing counts is a guess.
//!
//! Fixtures come from a real `mkfs.xfs` — the CI job installs xfsprogs
//! and builds them, and `scripts/vm-build-fixtures.sh` does it locally.
//! They are gitignored, so this skips rather than fails on a fresh
//! clone.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// A fixture that actually CONTAINS SOMETHING.
///
/// Not merely the first image found: most of the geometry fixtures are
/// a bare `mkfs.xfs` with an empty root, and a test that opens one and
/// finds nothing to compare passes while proving nothing. That is the
/// failure this function exists to prevent, so it mounts candidates and
/// returns the first whose root has a real entry.
fn fixture_with_content() -> Option<PathBuf> {
    let mut images: Vec<PathBuf> = std::fs::read_dir(share())
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "img"))
        .collect();
    images.sort();
    images.into_iter().find(|img| {
        let Ok(file) = FileDevice::open(img) else {
            return false;
        };
        let Ok(fs) = Filesystem::mount(Arc::new(file)) else {
            return false;
        };
        let Ok(root) = fs.root() else { return false };
        let Ok(entries) = root.entries() else {
            return false;
        };
        // A NON-EMPTY REGULAR FILE, not merely "some entry".
        //
        // "Has entries" was the test here, and it picked xfscreate-*,
        // whose 55 filler files are all zero length -- so the loop
        // skipped every one of them and the run ended on "no regular
        // file was read". That assertion did its job; this is the other
        // half of it. A fixture with nothing to read is not a fixture
        // this test can use, and choosing one is not something to
        // discover at the end.
        //
        // Recursive, because the file may be a level down: xfslog-*
        // keeps its files in logged/ and sf/, and the root holds only
        // directories.
        fn has_a_readable_file(fs: &Filesystem, path: &str, depth: u32) -> bool {
            if depth == 0 {
                return false;
            }
            let Ok(dir) = fs.open(path) else {
                return false;
            };
            let Ok(entries) = dir.entries() else {
                return false;
            };
            entries.iter().any(|e| {
                if e.name == b"." || e.name == b".." {
                    return false;
                }
                let name = String::from_utf8_lossy(&e.name);
                let child = if path == "/" {
                    format!("/{name}")
                } else {
                    format!("{path}/{name}")
                };
                match fs.open(&child) {
                    Ok(f) if f.is_regular_file() && !f.is_empty() => true,
                    Ok(f) if f.is_dir() => has_a_readable_file(fs, &child, depth - 1),
                    _ => false,
                }
            })
        }

        entries.iter().any(|e| e.name != b"." && e.name != b"..")
            && has_a_readable_file(&fs, "/", 4)
    })
}

/// Wraps a device and counts the reads that reach it.
///
/// The point of the handle is that a caller stops re-reading an inode
/// it already has. Counting is the only way to show that actually
/// happened rather than merely looking like it did.
struct CountingDevice {
    inner: Arc<dyn BlockRead>,
    reads: AtomicUsize,
    bytes: AtomicUsize,
}

impl CountingDevice {
    fn new(inner: Arc<dyn BlockRead>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            reads: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
        })
    }
    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }
    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
    fn reset(&self) {
        self.reads.store(0, Ordering::Relaxed);
        self.bytes.store(0, Ordering::Relaxed);
    }
}

impl BlockRead for CountingDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(buf.len(), Ordering::Relaxed);
        self.inner.read_at(offset, buf)
    }
    fn size_bytes(&self) -> u64 {
        self.inner.size_bytes()
    }
}

fn mount_counting(img: &Path) -> (Filesystem, Arc<CountingDevice>) {
    let file = FileDevice::open(img).expect("open the fixture");
    let counter = CountingDevice::new(Arc::new(file));
    let fs = Filesystem::mount(counter.clone()).expect("mount the fixture");
    (fs, counter)
}

/// Walk every directory, so the assertions below run against whatever
/// the fixture actually contains rather than a path assumed to exist.
fn walk(fs: &Filesystem, path: &str, out: &mut Vec<(String, bool)>) {
    let Ok(dir) = fs.open(path) else { return };
    let Ok(entries) = dir.entries() else { return };
    for e in entries {
        if e.name == b"." || e.name == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = if path == "/" {
            format!("/{name}")
        } else {
            format!("{path}/{name}")
        };
        let Ok(f) = fs.open(&child) else { continue };
        out.push((child.clone(), f.is_dir()));
        if f.is_dir() && out.len() < 64 {
            walk(fs, &child, out);
        }
    }
}

#[test]
fn open_agrees_with_the_low_level_calls_it_wraps() {
    let Some(img) = fixture_with_content() else {
        eprintln!("no fixture with content in .vm-share — skipping");
        return;
    };
    let (fs, _c) = mount_counting(&img);

    let mut found = Vec::new();
    walk(&fs, "/", &mut found);
    assert!(
        !found.is_empty(),
        "the fixture has no entries to check against"
    );

    let mut files_checked = 0;
    let mut dirs_checked = 0;
    for (path, is_dir) in &found {
        let handle = fs.open(path).expect("open through the handle");

        // The low-level route: resolve, then fetch the fork separately.
        let inode = fs.lookup_path(path).expect("lookup_path");
        let (low_inode, low_raw) = fs.read_inode_raw(inode.ino).expect("read_inode_raw");

        assert_eq!(
            handle.inode().ino,
            low_inode.ino,
            "{path}: handle resolved a different inode"
        );
        assert_eq!(handle.raw(), low_raw.as_slice(), "{path}: raw fork differs");

        if *is_dir {
            let a = handle.entries().expect("entries via handle");
            let b = fs.read_dir(&low_inode, &low_raw).expect("read_dir");
            assert_eq!(a.len(), b.len(), "{path}: entry count differs");
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(x.name, y.name, "{path}: entry name differs");
                assert_eq!(x.ino, y.ino, "{path}: entry inode differs");
            }
            dirs_checked += 1;
        } else if handle.is_regular_file() {
            let a = handle.read_all().expect("read_all via handle");
            let b = fs.read_file(&low_inode, &low_raw).expect("read_file");
            assert_eq!(a, b, "{path}: contents differ");
            assert_eq!(handle.len(), low_inode.size, "{path}: size differs");
            files_checked += 1;
        }
    }
    assert!(
        dirs_checked > 0,
        "no directory was compared — the test proved nothing"
    );
    eprintln!("compared {files_checked} files and {dirs_checked} directories");
}

/// The handle's ranged read must agree with the whole-file read on every
/// window, including the boundaries where an off-by-one hides: offset 0,
/// the last byte, and a request that runs past the end.
#[test]
fn ranged_reads_agree_with_the_whole_file() {
    let Some(img) = fixture_with_content() else {
        eprintln!("no fixture with content in .vm-share — skipping");
        return;
    };
    let (fs, _c) = mount_counting(&img);

    let mut found = Vec::new();
    walk(&fs, "/", &mut found);

    let mut checked = 0;
    for (path, is_dir) in &found {
        if *is_dir {
            continue;
        }
        let f = fs.open(path).expect("open");
        if !f.is_regular_file() || f.is_empty() {
            continue;
        }
        let whole = f.read_all().expect("read_all");
        let size = whole.len();

        for (offset, want) in [
            (0usize, size.min(1)),
            (0, size),
            (size / 2, size - size / 2),
            (size.saturating_sub(1), 1),
        ] {
            let mut buf = vec![0u8; want];
            let n = f.read_at(offset as u64, &mut buf).expect("read_at");
            assert_eq!(
                &buf[..n],
                &whole[offset..offset + n],
                "{path}: window at {offset} differs from the whole file"
            );
        }

        // Past the end returns nothing, rather than erroring or reading
        // stale bytes.
        let mut buf = [0u8; 16];
        let n = f.read_at(size as u64, &mut buf).expect("read past end");
        assert_eq!(n, 0, "{path}: reading at EOF should return 0 bytes");

        // Straddling the end returns only what exists.
        if size > 4 {
            let mut buf = vec![0u8; 64];
            let n = f.read_at((size - 4) as u64, &mut buf).expect("straddle");
            assert_eq!(n, 4, "{path}: a read straddling EOF should stop at it");
            assert_eq!(&buf[..4], &whole[size - 4..]);
        }
        checked += 1;
        if checked >= 8 {
            break;
        }
    }
    assert!(
        checked > 0,
        "no regular file was read — the test proved nothing"
    );
}

/// THE REASON THE TYPE EXISTS, measured rather than asserted.
///
/// `lookup_path` reads the raw fork at every path component and returns
/// only the parsed inode, so a caller that goes on to read the file
/// fetches the last inode a second time. `open` keeps it. This counts
/// the device reads on both routes and requires the handle to do fewer.
#[test]
fn the_handle_does_strictly_less_io_than_the_low_level_route() {
    let Some(img) = fixture_with_content() else {
        eprintln!("no fixture with content in .vm-share — skipping");
        return;
    };
    let (fs, counter) = mount_counting(&img);

    let mut found = Vec::new();
    walk(&fs, "/", &mut found);
    let target = found
        .iter()
        .find(|(p, is_dir)| !is_dir && fs.open(p).map(|f| f.is_regular_file()).unwrap_or(false))
        .map(|(p, _)| p.clone());
    // Deliberately a failure, not a skip. fixture_with_content() only
    // returns a populated image, so reaching here means the walk or the
    // handle is broken -- and a test that quietly passes on "nothing to
    // measure" is the exact thing this file is written to avoid.
    let path = target
        .expect("the fixture has entries but no regular file was reachable through the handle");

    // Route A: the handle. One walk, no re-read.
    counter.reset();
    let via_handle = fs.open(&path).expect("open").read_all().expect("read_all");
    let (handle_reads, handle_bytes) = (counter.reads(), counter.bytes());

    // Route B: resolve, then fetch the fork again, then read.
    counter.reset();
    let inode = fs.lookup_path(&path).expect("lookup_path");
    let (low_inode, low_raw) = fs.read_inode_raw(inode.ino).expect("read_inode_raw");
    let via_low = fs.read_file(&low_inode, &low_raw).expect("read_file");
    let (low_reads, low_bytes) = (counter.reads(), counter.bytes());

    assert_eq!(
        via_handle, via_low,
        "the two routes returned different bytes"
    );
    assert!(
        handle_reads < low_reads,
        "the handle was supposed to save a read: handle={handle_reads} reads \
         ({handle_bytes} bytes), low-level={low_reads} reads ({low_bytes} bytes)"
    );
    eprintln!(
        "{path}: handle {handle_reads} reads / {handle_bytes} bytes, \
         low-level {low_reads} reads / {low_bytes} bytes"
    );

    // Route C: the convenience wrapper. `read_path` used to be the
    // low-level sequence written out, and it now delegates to `open`.
    // Without this the saving is untested for the call most people
    // actually make -- verified by reverting `read_path` to its old
    // body and watching every other assertion here still pass.
    counter.reset();
    let via_path = fs.read_path(&path).expect("read_path");
    let path_reads = counter.reads();

    assert_eq!(via_path, via_handle, "read_path returned different bytes");
    assert_eq!(
        path_reads, handle_reads,
        "read_path should cost exactly what the handle costs ({handle_reads} reads),          not {path_reads} -- it is supposed to delegate rather than repeat the walk"
    );

    // Same for the directory wrapper.
    let dir = found
        .iter()
        .find(|(_, is_dir)| *is_dir)
        .map(|(p, _)| p.clone())
        .unwrap_or_else(|| "/".to_string());
    counter.reset();
    let via_open = fs.open(&dir).expect("open dir").entries().expect("entries");
    let open_reads = counter.reads();
    counter.reset();
    let via_list = fs.list_path(&dir).expect("list_path");
    let list_reads = counter.reads();
    assert_eq!(via_open.len(), via_list.len(), "{dir}: entry counts differ");
    assert_eq!(
        list_reads, open_reads,
        "{dir}: list_path should cost what the handle costs ({open_reads} reads),          not {list_reads}"
    );
}

/// `open_child` resolves within a directory rather than re-walking from
/// the root, so it must agree with the full-path open and do less work
/// the deeper the path is.
#[test]
fn open_child_agrees_with_a_full_path_open() {
    let Some(img) = fixture_with_content() else {
        eprintln!("no fixture with content in .vm-share — skipping");
        return;
    };
    let (fs, _c) = mount_counting(&img);

    let root = fs.root().expect("root");
    let entries = root.entries().expect("root entries");
    let mut checked = 0;
    for e in entries {
        if e.name == b"." || e.name == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = root.open_child(&e.name).expect("open_child");
        let direct = fs.open(&format!("/{name}")).expect("open by path");
        assert_eq!(
            child.inode().ino,
            direct.inode().ino,
            "/{name}: open_child and open disagree"
        );
        assert_eq!(child.raw(), direct.raw(), "/{name}: raw fork differs");
        checked += 1;
    }
    assert!(
        checked > 0,
        "the root directory was empty — nothing compared"
    );
}

/// A file is not a directory and a directory is not a file, and asking
/// for the wrong one is refused rather than answered with nonsense.
#[test]
fn the_handle_refuses_the_wrong_kind() {
    let Some(img) = fixture_with_content() else {
        eprintln!("no fixture with content in .vm-share — skipping");
        return;
    };
    let (fs, _c) = mount_counting(&img);

    let root = fs.root().expect("root");
    assert!(root.is_dir(), "the root must be a directory");
    assert!(
        root.open_child(b"definitely-not-here").is_err(),
        "a missing child must not resolve"
    );

    let mut found = Vec::new();
    walk(&fs, "/", &mut found);
    if let Some((path, _)) = found
        .iter()
        .find(|(p, d)| !d && fs.open(p).map(|f| f.is_regular_file()).unwrap_or(false))
    {
        let f = fs.open(path).expect("open");
        assert!(
            f.open_child(b"anything").is_err(),
            "{path}: a regular file has no children"
        );
    }
}
