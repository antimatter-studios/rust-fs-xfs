//! What a read costs, in calls to the device.
//!
//! # Why this is a test and not a benchmark
//!
//! The number that matters here is not wall time. Wall time on a laptop
//! with a warm page cache says more about the laptop than the driver:
//! run it twice and the second is faster for reasons this repository
//! does not control. **Calls to the device** are deterministic — the
//! same image walked the same way makes the same calls every time — so
//! they can be asserted on, and a change that makes the driver ask for
//! more is a regression a test can catch rather than a number somebody
//! has to remember.
//!
//! Wall time is printed beside them, because it is what a user feels,
//! and ignored by the assertions.
//!
//! # What is measured
//!
//! Three shapes, because they cost differently and a change can improve
//! one while ruining another:
//!
//! - **walk** — every directory in the tree, listed. Metadata only.
//! - **stat** — every file resolved by path from the root. Metadata,
//!   repeatedly, over the same blocks.
//! - **read** — every file's contents. Data, and the block map to
//!   find it.
//!
//! The second is the one a cache should transform: resolving `/a/b/c`
//! re-reads the root directory and every directory above the target,
//! once per path, and today every one of those is a fresh call to the
//! device.
//!
//! Fixtures are gitignored, so this skips on a fresh clone.

// EACH INTEGRATION TEST COMPILES `common` SEPARATELY, so everything in
// it that this one does not use -- which is the whole VM transport,
// since nothing here needs a kernel -- looks dead to the lint. It is
// used, by the oracles next door.
#[allow(dead_code)]
mod common;
use common::share;

use fs_core::{CountingDevice, FileDevice};
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

/// What one shape of read cost.
struct Cost {
    reads: u64,
    bytes: u64,
    micros: u128,
    /// How much work was actually done, so a number that fell because
    /// the driver did less is not read as a number that fell because
    /// the driver got better.
    items: usize,
}

fn fixture() -> Option<PathBuf> {
    // The largest tree available, since the cost of a walk is the point:
    // xfsstress corpora have the deepest, and the feature-matrix images
    // the widest.
    for name in [
        "xfsfeat-everything.img",
        "xfsdeep-inobt2.img",
        "xfs-4ags.img",
    ] {
        let p = share().join(name);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn mount_counting(img: &Path) -> (Filesystem, Arc<CountingDevice>) {
    let file = FileDevice::open(img).expect("open the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(file)));
    let fs = Filesystem::mount(counting.clone()).expect("mount");
    (fs, counting)
}

/// Every path in the tree, directories first, depth-bounded so a
/// pathological fixture cannot make this run forever.
fn walk_paths(fs: &Filesystem, at: &str, depth: u32, out: &mut Vec<(String, bool)>) {
    if depth == 0 || out.len() > 4000 {
        return;
    }
    let Ok(dir) = fs.open(at) else { return };
    let Ok(entries) = dir.entries() else { return };
    for e in entries {
        if e.name == b"." || e.name == b".." {
            continue;
        }
        let name = String::from_utf8_lossy(&e.name).to_string();
        let child = if at == "/" {
            format!("/{name}")
        } else {
            format!("{at}/{name}")
        };
        let Ok(f) = fs.open(&child) else { continue };
        let is_dir = f.is_dir();
        out.push((child.clone(), is_dir));
        if is_dir {
            walk_paths(fs, &child, depth - 1, out);
        }
    }
}

fn measure<F>(counting: &CountingDevice, items: usize, body: F) -> Cost
where
    F: FnOnce(),
{
    counting.reset();
    let start = Instant::now();
    body();
    Cost {
        reads: counting.reads(),
        bytes: counting.bytes(),
        micros: start.elapsed().as_micros(),
        items,
    }
}

fn report(what: &str, c: &Cost) {
    let per = if c.items == 0 {
        0.0
    } else {
        c.reads as f64 / c.items as f64
    };
    eprintln!(
        "{what:<6} {:>6} reads  {:>9} bytes  {:>8} µs  over {:>4} items  ({per:.1} reads/item)",
        c.reads, c.bytes, c.micros, c.items
    );
}

/// The measurement itself. Prints the numbers and asserts only that the
/// driver did the work — the figures are recorded in `docs/read-path-cost.md`
/// and compared by hand when something changes, because a threshold
/// baked in here would either be so loose it catches nothing or so tight
/// it fails on a fixture rebuild.
#[test]
fn what_a_read_costs_in_calls_to_the_device() {
    let Some(img) = fixture() else {
        eprintln!("no fixture to measure — skipping");
        return;
    };
    eprintln!("measuring {}", img.display());

    let (fs, counting) = mount_counting(&img);

    let mut paths = Vec::new();
    let walk = measure(&counting, 0, || walk_paths(&fs, "/", 8, &mut paths));
    let walk = Cost {
        items: paths.len(),
        ..walk
    };
    report("walk", &walk);

    let files: Vec<String> = paths
        .iter()
        .filter(|(_, is_dir)| !*is_dir)
        .map(|(p, _)| p.clone())
        .collect();

    // RESOLVING THE SAME PREFIXES AGAIN AND AGAIN is the shape a cache
    // is for: every path here walks the root and each directory above
    // its target, and each of those is a fresh call to the device today.
    let stat = measure(&counting, files.len(), || {
        for p in &files {
            let _ = fs.lookup_path(p);
        }
    });
    report("stat", &stat);

    let read = measure(&counting, files.len(), || {
        for p in &files {
            if let Ok(inode) = fs.lookup_path(p) {
                if let Ok((inode, raw)) = fs.read_inode_raw(inode.ino) {
                    let _ = fs.read_file(&inode, &raw);
                }
            }
        }
    });
    report("read", &read);

    assert!(
        !paths.is_empty(),
        "the fixture had nothing to walk — the measurement is of nothing"
    );
    assert!(
        walk.reads > 0 && stat.reads > 0,
        "no calls reached the device, so the counter is not wired to the mount"
    );
}
