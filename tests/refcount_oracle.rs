//! What happens to blocks two files share when one of them lets go.
//!
//! # Why this is not left to `xfs_repair`
//!
//! The feature matrix asks the checker whether the filesystem is sound,
//! and for the dangerous direction that works: freeing blocks another
//! file still points at gets caught, loudly.
//!
//! ```text
//! data fork in ino 134 claims free block 24
//! ```
//!
//! The opposite direction does not. A driver that decremented when it
//! should have freed would leak the blocks — nothing owns them, nothing
//! can allocate them, and the filesystem is entirely consistent. The
//! same blind spot cost the reverse-mapping tree a test: removing the
//! removal step changed no verdict anywhere.
//!
//! So this checks both ends against what the kernel does, which was
//! measured before any of it was written:
//!
//! ```text
//! one file          (no record — an unshared extent has none)
//! after the copy    [24,8,2,0]
//! first truncated   (no record)       and the free space is UNCHANGED
//! second truncated  free space gains  [24,25576]
//! ```

mod common;
use common::{kernel_run, share};

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let dir = share().join("scratch");
        std::fs::create_dir_all(&dir).expect("scratch directory");
        let path = dir.join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Self(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A reflink filesystem whose fixture really does share an extent —
/// `build-feature-matrix-fixtures.sh` makes `sf/shared.bin` a reflink
/// copy of `sf/data.bin`.
fn fixture() -> Option<PathBuf> {
    let p = share().join("xfsfeat-reflink.img");
    p.exists().then_some(p)
}

fn replay(img: &Path) -> bool {
    let name = img.file_name().unwrap().to_string_lossy().into_owned();
    let script = format!(
        r#"
        m=$(mktemp -d)
        mount -o loop,nouuid /share/scratch/{name} "$m" && umount "$m" || echo MOUNT_FAILED
        rmdir "$m" 2>/dev/null
        echo DONE
        "#
    );
    kernel_run(&script).is_some_and(|out| !out.contains("MOUNT_FAILED"))
}

fn open(img: &Path) -> Filesystem {
    Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount")
}

/// Whether any free extent covers `block`.
fn is_free(fs: &Filesystem, block: u32) -> bool {
    fs.free_extents(0)
        .expect("free space")
        .iter()
        .any(|e| e.startblock <= block && block < e.startblock + e.blockcount)
}

/// Truncating one of two sharers must leave the other's data alone, and
/// must not put the blocks back in free space.
#[test]
fn letting_go_of_a_shared_extent_leaves_the_blocks_with_the_other_file() {
    let Some(source) = fixture() else {
        eprintln!("no xfsfeat-reflink fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "refcount-share-scratch.img");
    let img = scratch.path();

    let (victim, survivor, block, wanted) = {
        let fs = open(img);
        let victim = fs.lookup_path("/sf/data.bin").expect("the file").ino;
        let survivor = fs
            .lookup_path("/sf/shared.bin")
            .expect("its reflink copy")
            .ino;
        let (inode, raw) = fs.read_inode_raw(survivor).expect("read it");
        let extents = fs.data_extents(&inode, &raw).expect("its extents");
        let (_, agblock) = fs.superblock().split_fsblock(extents[0].startblock);
        let wanted = fs.read_file(&inode, &raw).expect("its contents");
        (victim, survivor, agblock, wanted)
    };

    assert!(
        !open(img).refcount_records(0).unwrap().is_empty(),
        "the fixture must actually share an extent, or this proves nothing"
    );
    assert!(
        !is_free(&open(img), block),
        "the shared blocks are in use to begin with"
    );

    {
        let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(img).expect("rw")))
            .expect("mount read-write");
        fs.truncate_to_zero(victim)
            .expect("letting go of a shared extent is allowed; the blocks simply stay");
    }
    if !replay(img) {
        eprintln!("no kernel to replay the record — skipping the check");
        return;
    }

    let fs = open(img);
    assert!(
        !is_free(&fs, block),
        "group block {block} went back to free space while inode {survivor} still holds it — \
         the allocator would hand it out again"
    );

    // The record is gone because one owner is not sharing, which is what
    // the kernel does, and the surviving file is untouched.
    assert!(
        fs.refcount_records(0).unwrap().is_empty(),
        "with one owner left the extent is no longer shared and keeps no record"
    );
    let (inode, raw) = fs.read_inode_raw(survivor).expect("read the survivor");
    assert_eq!(
        fs.read_file(&inode, &raw).expect("its contents"),
        wanted,
        "the surviving file's bytes must be exactly what they were"
    );
}

/// And the last owner does free them — otherwise the blocks leak, which
/// leaves a perfectly consistent filesystem that has lost space.
#[test]
fn the_last_owner_gives_the_blocks_back() {
    let Some(source) = fixture() else {
        eprintln!("no xfsfeat-reflink fixture — skipping");
        return;
    };
    let scratch = Scratch::from(&source, "refcount-last-scratch.img");
    let img = scratch.path();

    let (first, second, block) = {
        let fs = open(img);
        let first = fs.lookup_path("/sf/data.bin").expect("the file").ino;
        let second = fs.lookup_path("/sf/shared.bin").expect("the copy").ino;
        let (inode, raw) = fs.read_inode_raw(first).expect("read it");
        let extents = fs.data_extents(&inode, &raw).expect("its extents");
        let (_, agblock) = fs.superblock().split_fsblock(extents[0].startblock);
        (first, second, agblock)
    };

    // One owner at a time: this driver logs one checkpoint per mount, so
    // each free is its own mount and its own replay.
    for ino in [first, second] {
        let fs = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(img).expect("rw")))
            .expect("mount read-write");
        fs.truncate_to_zero(ino).expect("truncate");
        drop(fs);
        if !replay(img) {
            eprintln!("no kernel to replay the record — skipping the check");
            return;
        }
    }

    assert!(
        is_free(&open(img), block),
        "both owners let go and group block {block} is still not free — the blocks leaked, \
         which no checker will complain about and nothing can ever allocate again"
    );
}
