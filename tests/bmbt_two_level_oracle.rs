//! A data fork whose B+tree has an interior level on disk reads the way
//! `xfs_db` maps it (#250).
//!
//! A file's extent map moves into a B+tree once the inode cannot hold it,
//! and the tree's root stays in the inode. At root level 1 every pointer in
//! the root names a leaf. `truncate_btree_fork.rs` builds that shape, and
//! nothing else in the suite goes deeper. At level 2 the root names interior
//! nodes on disk, whose own keys and pointers name the leaves, so
//! `bmbt::walk` has to descend through a node block it read rather than one
//! the inode holds. None of the fixtures had a map that deep, so that step
//! had never been checked against anything.
//!
//! #250 suspected exactly that step, pointing at the keys/pointers pairing
//! in an interior node. It turned out to be the reporter's read path, not
//! this parser. The coverage is still missing, and this test adds it.
//!
//! The kernel writes the file, one block at a time with a hole after each,
//! so every block is its own extent. Two independent readings of the result
//! are compared with the driver's:
//!
//! - `xfs_db`'s `bmap -d`, which walks the same tree with xfsprogs' code and
//!   prints every extent as file offset, packed start block and length, and
//!   `print u3.bmbt.level`, which says how deep the tree is; and
//! - the bytes that were written, which are known without reading anything
//!   back: 0xAB in every written block, zeros in every hole.

mod common;

use common::{kernel_run, scratch, share};

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "bmbt_two_level_oracle";

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// One-block pieces, each its own extent.
///
/// Sized for a root at level 2 with a wide margin. With `mkfs.xfs` defaults
/// (4 KiB blocks, 512-byte inodes), the in-inode root holds
/// `(336 - 4) / 16` = 20 pointers, and a leaf holds `(4096 - 72) / 16` = 251
/// records. After splitting, a leaf is between half full and full, so a
/// level-1 tree runs out somewhere between about 2,500 and 5,020 extents.
/// 12,000 is more than twice the larger figure. The depth is still asserted
/// from `xfs_db`, not assumed from this arithmetic.
const PIECES: u64 = 12_000;

/// The filesystem block size `mkfs.xfs` defaults to, and the size of each
/// piece written.
const BLOCK: u64 = 4096;

/// What each written block holds.
const FILL: u8 = 0xAB;

/// One extent as `xfs_db`'s `bmap -d` prints it:
/// `data offset 0 startblock 1234 (0/1234) count 1 flag 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Mapped {
    startoff: u64,
    startblock: u64,
    blockcount: u64,
    unwritten: bool,
}

fn parse_bmap_line(line: &str) -> Option<Mapped> {
    let words: Vec<&str> = line.split_whitespace().collect();
    // data offset N startblock N (a/b) count N flag N
    if words.len() != 10 || words[0] != "data" || words[1] != "offset" {
        return None;
    }
    let num = |i: usize| -> u64 {
        words[i].parse().unwrap_or_else(|_| {
            panic!(
                "xfs_db printed {:?} where a number was due: {line}",
                words[i]
            )
        })
    };
    assert_eq!(words[3], "startblock", "unexpected bmap line: {line}");
    assert_eq!(words[6], "count", "unexpected bmap line: {line}");
    assert_eq!(words[8], "flag", "unexpected bmap line: {line}");
    Some(Mapped {
        startoff: num(2),
        startblock: num(4),
        blockcount: num(7),
        unwritten: num(9) != 0,
    })
}

#[test]
fn a_two_level_bmap_tree_reads_as_xfs_db_maps_it() {
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volume
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volume beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let name = scratch.guest();

    // O_DIRECT (`xfs_io -d`) so each write allocates exactly the block it
    // covers. A buffered write would let speculative preallocation reach
    // across a hole, and a hole that got allocated is an extent that
    // should not be there. The commands are fed on stdin so this costs one
    // process rather than twelve thousand.
    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -b size={BLOCK} {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        awk 'BEGIN {{ for (i = 0; i < {PIECES}; i++) printf "pwrite -q -S {FILL:#x} %d {BLOCK}\n", i * 2 * {BLOCK} }}' \
            | xfs_io -f -d "$m/frag" && echo WRITE_OK
        ino=$(stat -c %i "$m/frag")
        echo "INO $ino"
        echo "SIZE $(stat -c %s "$m/frag")"
        # RETRIED ONCE. A busy unmount under a loaded runner is ordinary and
        # clears in a moment; one that does not leaves a volume xfs_db would
        # read mid-flight.
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo "FORMAT $(xfs_db -r -c "inode $ino" -c 'print core.format' {name})"
        echo "LEVEL $(xfs_db -r -c "inode $ino" -c 'print u3.bmbt.level' {name})"
        echo BMAP_BEGIN
        xfs_db -r -c "inode $ino" -c 'bmap -d' {name}
        echo BMAP_END
        echo DONE
        "#
    ));
    for step in ["MKFS_OK", "MOUNT_OK", "WRITE_OK"] {
        assert!(
            built.contains(step),
            "building the volume failed before {step}:\n{built}"
        );
    }
    assert!(
        !built.contains("UMOUNT_FAILED"),
        "the volume could not be unmounted, so xfs_db would read it mid-flight:\n{built}"
    );
    let guest = |key: &str| -> String {
        built
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{built}"))
            .trim()
            .to_string()
    };

    // THE SHAPE, FROM THE ORACLE. A level-1 tree is what the rest of the
    // suite already has, and this test would prove nothing new on one.
    assert_eq!(
        guest("FORMAT"),
        "core.format = 3 (btree)",
        "the file's map is not a B+tree"
    );
    let level: u16 = guest("LEVEL")
        .strip_prefix("u3.bmbt.level = ")
        .and_then(|l| l.parse().ok())
        .unwrap_or_else(|| panic!("xfs_db did not print the root's level:\n{built}"));
    assert!(
        level >= 2,
        "the root is at level {level}, so every pointer in it names a leaf and no \
         interior node on disk is read. Raise PIECES."
    );

    let expected: Vec<Mapped> = built
        .lines()
        .skip_while(|l| *l != "BMAP_BEGIN")
        .skip(1)
        .take_while(|l| *l != "BMAP_END")
        .filter_map(parse_bmap_line)
        .collect();
    assert_eq!(
        expected.len() as u64,
        PIECES,
        "xfs_db maps {} extents, not the {PIECES} one-block pieces written, so a hole \
         was allocated or two pieces merged:\n{}",
        expected.len(),
        built.lines().take(40).collect::<Vec<_>>().join("\n")
    );

    // THE DRIVER'S READING of the same tree.
    let dev = FileDevice::open(scratch.path()).expect("open the volume");
    let fs = Filesystem::mount(Arc::new(dev) as Arc<dyn BlockRead>).expect("mount");
    let ino: u64 = guest("INO").parse().expect("an inode number");
    let (inode, raw) = fs.read_inode_raw(ino).expect("the file's inode");
    assert_eq!(inode.format, fs_xfs::inode::Format::Btree);
    let found: Vec<Mapped> = fs
        .data_extents(&inode, &raw)
        .expect("walk the two-level map")
        .into_iter()
        .map(|e| Mapped {
            startoff: e.startoff,
            startblock: e.startblock,
            blockcount: e.blockcount,
            unwritten: e.unwritten,
        })
        .collect();
    if let Some(i) = (0..expected.len().min(found.len())).find(|&i| expected[i] != found[i]) {
        panic!(
            "extent {i} of {}: xfs_db maps {:?}, the driver read {:?}",
            expected.len(),
            expected[i],
            found[i]
        );
    }
    assert_eq!(
        found.len(),
        expected.len(),
        "the driver read {} extents and xfs_db maps {}",
        found.len(),
        expected.len()
    );

    // AND THE BYTES. Known from what was written, not from any reader.
    let size = (PIECES - 1) * 2 * BLOCK + BLOCK;
    assert_eq!(guest("SIZE"), size.to_string(), "the kernel's size");
    assert_eq!(inode.size, size, "the driver's size");
    let data = fs.read_file(&inode, &raw).expect("read the file whole");
    assert_eq!(data.len() as u64, size);
    for (i, block) in data.chunks(BLOCK as usize).enumerate() {
        let want = if i % 2 == 0 { FILL } else { 0 };
        if let Some(at) = block.iter().position(|&b| b != want) {
            panic!(
                "file block {i} holds {:#04x} at byte {at}, and it was written as \
                 {want:#04x} throughout",
                block[at]
            );
        }
    }
}
