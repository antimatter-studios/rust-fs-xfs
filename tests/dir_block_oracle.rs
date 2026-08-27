//! The block-form directory this driver builds must be the one the
//! kernel built, byte for byte.
//!
//! Converting a short-form directory is the one operation where "it
//! reads back correctly" is a weak claim. A block can list its entries
//! perfectly and still be wrong in ways only the next operation
//! notices: a hash index in the wrong order binary-searches to the wrong
//! place, a best-free array that lies about the largest gap sends the
//! next entry into space that is not free, and an entry whose tag does
//! not repeat its own offset breaks any walk that has to resynchronise.
//!
//! So the oracle is the kernel's own block. The same short-form
//! directory, the same added entry, the same target block — and the
//! bytes have to match.
//!
//! # The two fields that may differ, and why
//!
//! `crc` and `lsn` are stamped after logging: recovery recomputes the
//! checksum on write-out and assigns the sequence number, so a block
//! this driver writes into a record carries neither. Everything else is
//! compared exactly, and the test asserts that those two are the *only*
//! differences rather than masking a range and hoping.
//!
//! Fixtures are gitignored. Build them with
//! `./scripts/vm-build-dirconv-fixtures.sh`.

use fs_core::{BlockRead, FileDevice};
use fs_xfs::dir;
use fs_xfs::dir_block::{self, Entry};
use fs_xfs::format::attr::hashname;
use fs_xfs::format::dir::{offsets, XFS_DIR2_BLOCK_TAIL_SIZE, XFS_DIR2_LEAF_ENTRY_SIZE};
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory each fixture converts.
const DIR: &str = "/d";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

/// Every name in a directory, with its inode and type byte.
fn listing(img: &Path) -> Vec<Entry> {
    let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
    let found = fs.lookup_path(DIR).expect("the directory is there");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("read it");
    fs.read_dir(&inode, &raw)
        .expect("list it")
        .into_iter()
        .map(|e| Entry {
            name: e.name,
            ino: e.ino,
            ftype: dir::ftype_to_raw(e.ftype),
        })
        .collect()
}

/// Build the block for `case` and compare it against the kernel's.
fn compare(case: &str) -> Option<()> {
    let before_path = share().join(format!("xfsdirconv-{case}-before.img"));
    let after_path = share().join(format!("xfsdirconv-{case}-after.img"));
    if !before_path.exists() || !after_path.exists() {
        return None;
    }

    // What the directory held before, and in what order.
    let before_dev = Arc::new(FileDevice::open(&before_path).expect("open before"));
    let before_fs = Filesystem::mount(before_dev).expect("mount before");
    let before_found = before_fs.lookup_path(DIR).expect("the directory");
    let (before_inode, before_raw) = before_fs
        .read_inode_raw(before_found.ino)
        .expect("read the directory inode");
    assert_eq!(
        before_inode.format,
        fs_xfs::inode::Format::Local,
        "{case}: the before image's directory should still be short form"
    );
    let sb = before_fs.superblock().clone();
    let (fork_start, fork_end) = before_inode.data_fork_range(usize::from(sb.inodesize));
    let parsed = dir::read_short_form(&before_inode, &before_raw[fork_start..fork_end], &sb)
        .expect("parse the short form");

    // The entries that were added. In order, because the order they were
    // added in is the order the kernel placed them.
    let before_names: Vec<Vec<u8>> = parsed.entries.iter().map(|e| e.name.clone()).collect();
    let added: Vec<Entry> = listing(&after_path)
        .into_iter()
        .filter(|e| !before_names.contains(&e.name))
        .collect();
    assert!(
        !added.is_empty(),
        "{case}: nothing was added between the two images"
    );

    // Where the kernel put the block, and what it wrote there.
    let after_dev = Arc::new(FileDevice::open(&after_path).expect("open after"));
    let after_fs = Filesystem::mount(after_dev.clone()).expect("mount after");
    let after_found = after_fs.lookup_path(DIR).expect("the directory");
    let (after_inode, after_raw) = after_fs
        .read_inode_raw(after_found.ino)
        .expect("read the directory inode");
    assert_eq!(
        after_inode.format,
        fs_xfs::inode::Format::Extents,
        "{case}: the after image's directory should have been converted"
    );
    let extent = after_fs
        .data_extents(&after_inode, &after_raw)
        .expect("its extents")[0];

    let dirblocksize = (u64::from(sb.blocksize) << sb.dirblklog) as usize;
    let mut theirs = vec![0u8; dirblocksize];
    after_dev
        .read_at(sb.fsblock_offset(extent.startblock), &mut theirs)
        .expect("read the kernel's block");

    // Ours, from the short form plus what was added.
    let mut entries = dir_block::entries_from_short_form(&parsed, before_found.ino, None);
    entries.extend(added);
    let ours = dir_block::build(&sb, extent.startblock, before_found.ino, &entries)
        .unwrap_or_else(|e| panic!("{case}: building the block: {e}"));

    assert_eq!(ours.len(), theirs.len(), "{case}: block sizes differ");

    // The two fields stamped after logging.
    use offsets::dir3_blk as h;
    let stamped: Vec<usize> = (h::CRC..h::CRC + 4).chain(h::LSN..h::LSN + 8).collect();

    let differing: Vec<usize> = (0..ours.len()).filter(|&i| ours[i] != theirs[i]).collect();
    let unexpected: Vec<usize> = differing
        .iter()
        .copied()
        .filter(|i| !stamped.contains(i))
        .collect();

    if !unexpected.is_empty() {
        let first = unexpected[0];
        let from = first.saturating_sub(16);
        let to = (first + 16).min(ours.len());
        panic!(
            "{case}: {} bytes differ outside the checksum and sequence number, \
             first at {first}.\n  ours:   {:02x?}\n  kernel: {:02x?}",
            unexpected.len(),
            &ours[from..to],
            &theirs[from..to]
        );
    }

    eprintln!(
        "  {case}: {} entries, block matches the kernel's byte for byte \
         ({} stamped bytes excluded)",
        entries.len(),
        differing.len()
    );
    Some(())
}

/// Every conversion fixture, compared against the kernel's own block.
#[test]
fn the_block_matches_what_the_kernel_built() {
    let mut ran = Vec::new();
    for case in ["exact", "spill"] {
        match compare(case) {
            Some(()) => ran.push(case),
            None => eprintln!("{case}: fixture missing — skipped"),
        }
    }
    if ran.is_empty() {
        eprintln!(
            "no conversion fixtures; build them with \
             ./scripts/vm-build-dirconv-fixtures.sh"
        );
        return;
    }
    eprintln!("{} conversion(s) match the kernel byte for byte", ran.len());
}

/// This driver's own reader must accept the block this driver built.
///
/// A weaker check than matching the kernel, but it fails differently:
/// the reader verifies the tags, the index ordering and the stale count,
/// so a block that matched the kernel everywhere except one of those
/// would be caught here with a message about which.
#[test]
fn our_reader_accepts_our_own_block() {
    let before_path = share().join("xfsdirconv-exact-before.img");
    if !before_path.exists() {
        eprintln!("no conversion fixture — skipping");
        return;
    }

    let dev = Arc::new(FileDevice::open(&before_path).expect("open"));
    let fs = Filesystem::mount(dev).expect("mount");
    let found = fs.lookup_path(DIR).expect("the directory");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("read it");
    let sb = fs.superblock().clone();
    let (start, end) = inode.data_fork_range(usize::from(sb.inodesize));
    let parsed = dir::read_short_form(&inode, &raw[start..end], &sb).expect("short form");

    let entries = dir_block::entries_from_short_form(&parsed, found.ino, None);
    let block = dir_block::build(&sb, 15, found.ino, &entries).expect("build");

    let read_back = dir::parse_block_form(&block, &sb)
        .expect("this driver's reader must accept the block it just built");

    // `.` and `..` are entries in this form, and the reader returns them.
    assert_eq!(
        read_back.entries.len(),
        entries.len(),
        "every entry written should be read back"
    );
    assert_eq!(
        read_back.index.len(),
        entries.len(),
        "one index record per entry"
    );
    for (written, got) in entries.iter().zip(&read_back.entries) {
        assert_eq!(written.name, got.name);
        assert_eq!(written.ino, got.ino);
    }
}

/// `hashname` must agree with the hashes the kernel put in a real index.
///
/// The function was reconstructed from the outside — the specification
/// names it but does not give it — and was checked against twenty-three
/// samples taken from a disk examiner. It had never been checked against
/// a hash a kernel actually wrote into a filesystem.
///
/// That matters now rather than before: reading a directory never needed
/// the hash, because a reader can walk the entries and compare names.
/// Writing one does. An index built from a wrong hash is sorted
/// correctly, passes every structural check, and cannot be searched.
#[test]
fn our_hash_agrees_with_the_kernels_index() {
    let path = share().join("xfsdirconv-exact-after.img");
    if !path.exists() {
        eprintln!("no conversion fixture — skipping");
        return;
    }

    let dev = Arc::new(FileDevice::open(&path).expect("open"));
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let found = fs.lookup_path(DIR).expect("the directory");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("read it");
    let sb = fs.superblock().clone();
    let extent = fs.data_extents(&inode, &raw).expect("extents")[0];

    let dirblocksize = (u64::from(sb.blocksize) << sb.dirblklog) as usize;
    let mut block = vec![0u8; dirblocksize];
    dev.read_at(sb.fsblock_offset(extent.startblock), &mut block)
        .expect("read the block");

    let be32 = |at: usize| u32::from_be_bytes(block[at..at + 4].try_into().unwrap());

    let tail_at = dirblocksize - XFS_DIR2_BLOCK_TAIL_SIZE;
    let count = be32(tail_at + offsets::block_tail::COUNT) as usize;
    let index_start = tail_at - count * XFS_DIR2_LEAF_ENTRY_SIZE;

    let mut checked = 0usize;
    for i in 0..count {
        let at = index_start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        let kernel_hash = be32(at + offsets::leaf_entry::HASHVAL);
        let addr = be32(at + offsets::leaf_entry::ADDRESS) as usize * 8;
        if addr == 0 || addr >= index_start {
            continue;
        }
        let namelen = block[addr + offsets::data_entry::NAMELEN] as usize;
        let name =
            &block[addr + offsets::data_entry::NAME..addr + offsets::data_entry::NAME + namelen];
        assert_eq!(
            hashname(name),
            kernel_hash,
            "hashname disagrees with the kernel for {:?}",
            String::from_utf8_lossy(name)
        );
        checked += 1;
    }

    assert!(
        checked > 10,
        "only {checked} names were checked, which is too few to be evidence"
    );
    // `.` and `..` are in there, and they are the two shortest names a
    // directory can hold — the tail cases of a function that consumes
    // four bytes at a time.
    eprintln!("hashname agrees with the kernel on {checked} names, `.` and `..` included");
}
