//! Read filesystems this driver had no hand in shaping.
//!
//! Every other fixture here is a tree somebody sat down and thought of,
//! which means it is a tree shaped by the same assumptions the driver
//! was written under. It will never contain the case nobody thought of.
//!
//! These two were shaped by stress generators instead: long randomised
//! sequences of filesystem operations, run against a mounted filesystem
//! by the kernel. What comes out is far more awkward than anything worth
//! writing by hand — 12-deep directory paths, hundreds of device nodes,
//! sparse files, files whose extents run into the dozens — and it was
//! produced without reference to what this driver happens to find easy.
//!
//! # What is being compared
//!
//! Not this driver against itself. Each fixture carries a manifest
//! generated **inside Linux by the kernel's own XFS driver**, on a
//! read-only mount, listing every path with its type, size and either
//! its symlink target or the SHA-256 of its contents. This walks the
//! same image with this driver and requires the two to agree entry for
//! entry.
//!
//! Nothing in this repository decides what the right answer is. The
//! generator decided what went on the disk and the kernel said what is
//! there; a disagreement is this driver's.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-stress-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::format::symlink::buf_space;
use fs_xfs::inode::{FileType, Format, Inode};
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// One entry as either side describes it.
#[derive(Debug, PartialEq, Eq)]
struct Entry {
    /// `dir`, `file`, `link`, `chr`, `blk`, `fifo` or `sock` — the same
    /// words the manifest uses, so a mismatch reads plainly.
    kind: String,
    /// Bytes for a file, zero for everything else.
    size: u64,
    /// A symlink's target, a file's SHA-256, or `-`.
    detail: String,
}

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// The manifest's word for what an inode is.
///
/// A device node's `size` is its device number, not a length, so
/// everything that is not a file reports zero — matching what the
/// manifest records and keeping the comparison about structure.
fn kind_of(inode: &Inode) -> Option<&'static str> {
    Some(match inode.file_type()? {
        FileType::Directory => "dir",
        FileType::Regular => "file",
        FileType::Symlink => "link",
        FileType::CharDevice => "chr",
        FileType::BlockDevice => "blk",
        FileType::Fifo => "fifo",
        FileType::Socket => "sock",
    })
}

/// Hash a file without holding it in memory.
///
/// The stress fixtures hold hundreds of megabytes of logical file
/// bytes — most of it holes — so reading each file whole would cost far
/// more than the comparison is worth.
fn sha256_of(fs: &Filesystem, inode: &Inode, raw: &[u8]) -> String {
    const CHUNK: usize = 1 << 20;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut at = 0u64;
    while at < inode.size {
        let want = CHUNK.min((inode.size - at) as usize);
        let got = fs
            .read_at(inode, raw, at, &mut buf[..want])
            .unwrap_or_else(|e| panic!("read at {at}: {e}"));
        assert!(got > 0, "a read inside the file returned nothing at {at}");
        hasher.update(&buf[..got]);
        at += got as u64;
    }
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Walk the whole tree, as this driver sees it.
fn walk(fs: &Filesystem) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let root = fs.superblock().rootino;
    let mut queue = vec![(String::new(), root)];

    while let Some((prefix, ino)) = queue.pop() {
        let (inode, raw) = fs
            .read_inode_raw(ino)
            .unwrap_or_else(|e| panic!("{prefix}: read inode {ino}: {e}"));
        for e in fs
            .read_dir(&inode, &raw)
            .unwrap_or_else(|err| panic!("{prefix}: read directory {ino}: {err}"))
        {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let path = format!("{prefix}/{}", String::from_utf8_lossy(&e.name));
            let (child, craw) = fs
                .read_inode_raw(e.ino)
                .unwrap_or_else(|err| panic!("{path}: read inode {}: {err}", e.ino));

            let Some(kind) = kind_of(&child) else {
                panic!("{path}: inode {} has an unrecognised mode", e.ino);
            };
            let (size, detail) = match kind {
                "file" => (child.size, sha256_of(fs, &child, &craw)),
                "link" => (
                    0,
                    String::from_utf8_lossy(
                        &fs.read_link(&child, &craw)
                            .unwrap_or_else(|err| panic!("{path}: read link: {err}")),
                    )
                    .into_owned(),
                ),
                _ => (0, "-".to_string()),
            };
            if kind == "dir" {
                queue.push((path.clone(), e.ino));
            }
            out.insert(
                path,
                Entry {
                    kind: kind.to_string(),
                    size,
                    detail,
                },
            );
        }
    }
    out
}

/// The manifest, as the kernel wrote it.
fn manifest(path: &Path) -> BTreeMap<String, Entry> {
    let text = std::fs::read_to_string(path).expect("read the manifest");
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split('\t');
            let path = f.next().expect("path").to_string();
            let kind = f.next().unwrap_or_default().to_string();
            let size = f.next().unwrap_or("0").parse().unwrap_or(0);
            let detail = f.next().unwrap_or("-").to_string();
            (path, Entry { kind, size, detail })
        })
        .collect()
}

/// Compare one fixture against its manifest, reporting what differs
/// rather than only that something does.
fn check(name: &str) -> Option<usize> {
    let img = share().join(format!("{name}.img"));
    let man = share().join(format!("{name}.manifest"));
    if !img.exists() || !man.exists() {
        eprintln!("{name}: no fixture — skipping");
        return None;
    }

    let theirs = manifest(&man);
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).expect("open")))
        .unwrap_or_else(|e| panic!("{name}: this driver will not mount the fixture: {e}"));
    let ours = walk(&fs);

    let missing: Vec<_> = theirs.keys().filter(|k| !ours.contains_key(*k)).collect();
    let extra: Vec<_> = ours.keys().filter(|k| !theirs.contains_key(*k)).collect();
    assert!(
        missing.is_empty(),
        "{name}: {} paths the kernel lists are not reachable through this driver, \
         starting with {:?}",
        missing.len(),
        &missing[..missing.len().min(5)]
    );
    assert!(
        extra.is_empty(),
        "{name}: this driver reports {} paths the kernel does not, starting with {:?}",
        extra.len(),
        &extra[..extra.len().min(5)]
    );

    let mut wrong = Vec::new();
    for (path, want) in &theirs {
        let got = &ours[path];
        if got != want {
            wrong.push(format!(
                "  {path}\n    kernel: {want:?}\n    ours:   {got:?}"
            ));
        }
    }
    assert!(
        wrong.is_empty(),
        "{name}: {} entries differ from what the kernel reports:\n{}",
        wrong.len(),
        wrong[..wrong.len().min(8)].join("\n")
    );

    Some(theirs.len())
}

/// A tree built by a long randomised sequence of filesystem operations.
#[test]
fn a_stress_generated_tree_reads_back_exactly() {
    let Some(n) = check("xfsstress-ops") else {
        return;
    };
    eprintln!("xfsstress-ops: {n} entries agree with the kernel");
    // The fixture's value is in what a hand-written tree never contains.
    // If it ever shrinks to a handful of entries, it has stopped being
    // that and the agreement stops meaning much.
    assert!(n > 100, "the fixture holds only {n} entries");
}

/// The same tree on 1 KiB blocks, where a long symlink target no longer
/// fits in one block.
///
/// Reassembling a target from several blocks is a distinct path with its
/// own way of going wrong — a block taken out of order builds a target
/// of exactly the right length and the wrong destination. At 4 KiB the
/// generator's longest target still fits one block, so that path never
/// runs and this fixture is what exercises it.
#[test]
fn a_multi_block_symlink_target_reads_back_exactly() {
    let name = "xfsstress-ops1k";
    let Some(n) = check(name) else {
        return;
    };

    // Not an aside: without a target that actually spans blocks, this
    // test is the previous one at a different block size.
    let img = share().join(format!("{name}.img"));
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&img).expect("open"))).expect("mount");
    // One block's worth. A target longer than this cannot be held by a
    // single-block extent, so it is the threshold that says whether the
    // multi-block path was taken at all.
    let per_block = buf_space(fs.superblock().blocksize as usize, fs.superblock().is_v5());

    let mut spanning = 0usize;
    let mut longest = 0usize;
    let mut queue = vec![fs.superblock().rootino];
    while let Some(ino) = queue.pop() {
        let (inode, raw) = fs.read_inode_raw(ino).expect("read inode");
        for e in fs.read_dir(&inode, &raw).expect("read directory") {
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let (child, craw) = fs.read_inode_raw(e.ino).expect("read inode");
            match child.file_type() {
                Some(FileType::Directory) => queue.push(e.ino),
                Some(FileType::Symlink) if child.format != Format::Local => {
                    let target = fs.read_link(&child, &craw).expect("read link");
                    longest = longest.max(target.len());
                    if target.len() > per_block {
                        spanning += 1;
                    }
                }
                _ => {}
            }
        }
    }

    eprintln!(
        "{name}: {n} entries agree; {spanning} symlink targets span more than one          {per_block}-byte block, longest {longest}"
    );
    assert!(
        spanning > 0,
        "no target exceeds one block's {per_block} bytes (longest was {longest}), so the          multi-block path this fixture exists for was never taken"
    );
}

/// A single file hammered with randomised reads, writes, truncates,
/// hole punches and mmap operations.
#[test]
fn a_stress_hammered_file_reads_back_exactly() {
    let Some(n) = check("xfsstress-fsx") else {
        return;
    };
    eprintln!("xfsstress-fsx: {n} entries agree with the kernel");
}
