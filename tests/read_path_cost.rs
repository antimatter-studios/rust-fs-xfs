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

/// The fixtures worth measuring, widest first: the cost of a walk is the
/// point, so what matters is how much tree the image holds.
///
/// EVERY ONE OF THESE IS POPULATED. A bare geometry image is not here, and
/// that is the whole of this list's job: `xfs-4ags.img` used to be the last
/// resort, and a run that fell through to it measured an empty filesystem
/// and then failed with "the fixture had nothing to walk" — a true
/// statement about the wrong thing, since what had gone wrong was that the
/// populated fixtures were never built (#213).
const POPULATED: [&str; 3] = [
    "xfsfeat-everything.img",
    "xfsdeep-inobt2.img",
    "xfsdata-default.img",
];

fn fixture() -> Option<PathBuf> {
    POPULATED
        .iter()
        .map(|name| share().join(name))
        .find(|path| path.exists())
}

/// The counter sits BELOW the cache, so what it reports is what
/// actually reached the device rather than what the driver asked for.
/// `blocks` of zero mounts without a cache, which is the baseline.
fn mount_counting(img: &Path, blocks: usize) -> (Filesystem, Arc<CountingDevice>) {
    let file = FileDevice::open(img).expect("open the fixture");
    let counting = Arc::new(CountingDevice::new(Arc::new(file)));
    let fs = Filesystem::mount_with_cache(counting.clone(), blocks).expect("mount");
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
        eprintln!(
            "no populated fixtures ({}) — skipping. Build them with \
             ./scripts/build-feature-matrix-fixtures.sh, ./scripts/build-deeptree-fixtures.sh \
             or ./scripts/build-data-fixtures.sh",
            POPULATED.join(", ")
        );
        return;
    };
    eprintln!("measuring {}", img.display());

    eprintln!("--- uncached ---");
    let uncached = measure_one(&img, 0);
    eprintln!("--- cached ---");
    let cached = measure_one(&img, 512);

    // THE ASSERTIONS ARE ON THE UNCACHED PASS, because it is the one
    // that must reach the device: if the counter reports nothing there,
    // it is not wired to the mount and every figure above is fiction.
    // The cached pass is allowed to reach zero -- it does, for `stat`,
    // once the cache holds the directories every path walks through --
    // so asserting on it would be asserting that the cache failed.
    assert!(
        uncached.walk.items > 0,
        "{} is populated and yet the walk found nothing, so the walk is not walking",
        img.display()
    );
    assert!(
        uncached.walk.reads > 0 && uncached.stat.reads > 0,
        "no calls reached the device, so the counter is not wired to the mount"
    );
    // THE READ PASS REACHED THE DEVICE. Safe here and not in every sibling:
    // this driver reads file data on demand, and `docs/read-path-cost.md`
    // records the uncached read at 522 calls and 754 KB. (btrfs loads its
    // tree at mount and legitimately records zero for one fixture.)
    //
    // ONLY WHEN THE FILES HAD BYTES TO RETURN. A freshly formatted fixture
    // (`xfs-4ags.img`, the fallback) holds no regular files, so a correct
    // driver reads nothing there (Greptile on #177).
    assert!(
        uncached.read_returned == 0 || uncached.read.bytes > 0,
        "the read pass returned {} bytes and fetched none from the device",
        uncached.read_returned
    );
    for (what, un, ca) in [
        ("walk", &uncached.walk, &cached.walk),
        ("stat", &uncached.stat, &cached.stat),
        ("read", &uncached.read, &cached.read),
    ] {
        assert!(
            ca.reads <= un.reads,
            "{what}: the cache made it ask for more ({} vs {})",
            ca.reads,
            un.reads
        );
        assert_eq!(
            ca.items, un.items,
            "{what}: the two passes did different amounts of work, so the \
             figures are not comparable"
        );
    }
}

/// What one pass measured, so the two can be compared rather than each
/// asserting on itself.
struct Pass {
    walk: Cost,
    stat: Cost,
    read: Cost,
    /// Bytes the read pass's `read_file` calls returned.
    read_returned: u64,
}

fn measure_one(img: &Path, blocks: usize) -> Pass {
    let (fs, counting) = mount_counting(img, blocks);

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
    //
    // `items` FOR STAT AND READ COUNTS CALLS THAT SUCCEEDED (#163). Both
    // loops discarded their results, so a pass that failed before any I/O
    // recorded a cheaper cost, and `items` was `files.len()` in both
    // passes whatever happened. A failure now panics, and what the reads
    // returned is checked against the sizes the lookups declared.
    let mut resolved = 0usize;
    let mut regular = 0usize;
    let mut declared = 0u64;
    let stat = measure(&counting, 0, || {
        for p in &files {
            match fs.lookup_path(p) {
                Ok(inode) => {
                    resolved += 1;
                    if inode.is_regular_file() {
                        regular += 1;
                        declared += inode.size;
                    }
                }
                Err(e) => panic!("lookup_path({p}) failed during the measurement: {e:?}"),
            }
        }
    });
    let stat = Cost {
        items: resolved,
        ..stat
    };
    report("stat", &stat);

    let mut read_ok = 0usize;
    let mut returned = 0u64;
    let read = measure(&counting, 0, || {
        for p in &files {
            let inode = fs
                .lookup_path(p)
                .unwrap_or_else(|e| panic!("lookup_path({p}) failed during the read pass: {e:?}"));
            // Regular files only: a symlink's target is `read_link`'s, not
            // `read_file`'s, and a device node has no data to read.
            if !inode.is_regular_file() {
                continue;
            }
            let (inode, raw) = fs
                .read_inode_raw(inode.ino)
                .unwrap_or_else(|e| panic!("read_inode_raw({p}) failed: {e:?}"));
            let bytes = fs
                .read_file(&inode, &raw)
                .unwrap_or_else(|e| panic!("read_file({p}) failed: {e:?}"));
            read_ok += 1;
            returned += bytes.len() as u64;
        }
    });
    let read = Cost {
        items: read_ok,
        ..read
    };
    report("read", &read);
    assert_eq!(
        (resolved, read_ok),
        (files.len(), regular),
        "not every walked file was resolved, or not every regular file read"
    );
    assert_eq!(
        returned, declared,
        "the reads returned {returned} bytes where the files declare {declared}"
    );

    Pass {
        walk,
        stat,
        read,
        read_returned: returned,
    }
}
