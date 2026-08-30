//! Cross-validation of the directory parser against real filesystems.
//!
//! Same contract as `tests/oracle_vm_fixtures.rs`, applied to
//! directories: filesystems are built by the canonical `mkfs.xfs`,
//! populated, and dumped with `xfs_db`; this driver parses the same
//! images on the host and every field it reports must match the value
//! the reference debugger reports for that field.
//!
//! That is the only kind of test that can establish this module is
//! *correct*. The unit tests in `src/dir.rs` parse fixtures the crate
//! built itself, which proves self-consistency and nothing more — a
//! misreading of the format gets baked into the builder and the parser
//! alike, and the two agree with each other while disagreeing with every
//! real filesystem. Three bugs have already shipped past a green unit
//! suite here for exactly that reason. See AGENTS.md.
//!
//! # What each test establishes
//!
//! - [`root_directory_agrees_with_xfs_db`] compares the short-form
//!   parser field by field against `xfs_db`'s `u3.sfdir3` rendering:
//!   the header counts, the parent inode, and every entry's name
//!   length, offset cookie, name, inode number and file type. This is
//!   what pins down the short-form layout — in particular that the file
//!   type byte sits between the name and the inode number, and that the
//!   offset is a plain big-endian `u16`.
//! - [`root_directory_parses_on_real_images`] runs the parser over
//!   every image, including the ones with empty roots.
//! - [`directory_blocks_in_real_images_parse_and_verify`] finds the
//!   block-, leaf- and node-form directory blocks by scanning the image
//!   for their magics, then verifies and parses each one. It needs no
//!   extent map, which this module does not own, and it validates the
//!   v5 self-describing header against blocks a real kernel wrote.
//! - [`v4_fixture_advertises_ftype_outside_the_incompat_mask`] pins the
//!   one superblock fact this module cannot get from
//!   `Superblock::has_ftype`.
//!
//! Fixtures are gitignored and absent on a fresh clone, so these tests
//! skip rather than fail when `.vm-share` is empty. Generate them with:
//!
//! ```sh
//! ./scripts/vm.sh up
//! ./scripts/vm-build-fixtures.sh
//! ```

use fs_core::FileDevice;
use fs_xfs::dir::{self, DirEntry};
use fs_xfs::endian::be64;
use fs_xfs::inode::{FileType, Format, Inode};
use fs_xfs::superblock::Superblock;
use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Locate every `.img` in `.vm-share`, paired with its `.rootdump` when
/// one exists. Images without a dump have an empty root directory.
fn images() -> Vec<(String, PathBuf, Option<PathBuf>)> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let Ok(entries) = std::fs::read_dir(&share) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let name = p.file_stem().unwrap().to_string_lossy().into_owned();
        let dump = p.with_extension("rootdump");
        let dump = dump.exists().then_some(dump);
        out.push((name, p, dump));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Byte offset of an inode within the image, derived from its number.
///
/// The same derivation `tests/oracle_vm_fixtures.rs` uses: an inode
/// number packs its allocation group, the AG-relative block, and the
/// slot within that block into one value.
fn inode_offset(sb: &Superblock, ino: u64) -> usize {
    let (ag, ag_block, offset) = sb.split_ino(ino);
    let bytes = u64::from(ag) * u64::from(sb.agblocks) * u64::from(sb.blocksize)
        + u64::from(ag_block) * u64::from(sb.blocksize)
        + u64::from(offset) * u64::from(sb.inodesize);
    bytes as usize
}

/// Byte offset of a filesystem block number within the image.
///
/// An `xfs_fsblock_t` packs the allocation group into its high bits the
/// way an inode number does, but splits at `agblklog` with no inode
/// slot below it.
fn fsb_to_byte(sb: &Superblock, fsb: u64) -> u64 {
    let ag = fsb >> sb.agblklog;
    let block = fsb & ((1u64 << sb.agblklog) - 1);
    (ag * u64::from(sb.agblocks) + block) * u64::from(sb.blocksize)
}

/// Read an image's superblock and root inode, and hand back the byte
/// range of the inode's data fork **within the image**.
///
/// `Inode::data_fork_range` is relative to the start of the inode
/// record, so it has to be shifted by the inode's own offset before it
/// can index the image. Getting that wrong reads a neighbouring inode's
/// fork, which is exactly the kind of plausible-looking garbage this
/// crate's identity checks exist to catch.
fn root_dir_fork(bytes: &[u8], label: &str) -> (Superblock, Inode, Range<usize>) {
    let sb = Superblock::parse(bytes)
        .unwrap_or_else(|e| panic!("{label}: failed to parse the superblock: {e}"));
    let off = inode_offset(&sb, sb.rootino);
    let inode = Inode::parse(&bytes[off..], &sb, sb.rootino)
        .unwrap_or_else(|e| panic!("{label}: failed to parse the root inode: {e}"));
    let (start, end) = inode.data_fork_range(usize::from(sb.inodesize));
    (sb, inode, off + start..off + end)
}

// ---------------------------------------------------------------------
// The xfs_db root directory dump
//
// `xfs_db -c 'inode <rootino>' -c print` renders a short-form directory
// as a flat list of assignments:
//
//     u3.sfdir3.hdr.count = 8
//     u3.sfdir3.hdr.i8count = 0
//     u3.sfdir3.hdr.parent.i4 = 128
//     u3.sfdir3.list[0].namelen = 9
//     u3.sfdir3.list[0].offset = 0x60
//     u3.sfdir3.list[0].name = "small.txt"
//     u3.sfdir3.list[0].inumber.i4 = 131
//     u3.sfdir3.list[0].filetype = 1
//
// The `u3.sfdir3` prefix varies — a v4 filesystem renders `u.sfdir2`,
// and a directory holding wide inode numbers renders `inumber.i8` — so
// the parser below keys off the suffix rather than the whole path.
// ---------------------------------------------------------------------

/// One entry as the reference debugger reports it.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct OracleEntry {
    namelen: Option<u64>,
    offset: Option<u64>,
    name: Option<String>,
    ino: Option<u64>,
    ftype: Option<u64>,
}

/// A whole short-form directory as the reference debugger reports it.
#[derive(Debug, Default)]
struct RootDump {
    rootino: Option<u64>,
    count: Option<u64>,
    i8count: Option<u64>,
    parent: Option<u64>,
    entries: BTreeMap<usize, OracleEntry>,
}

/// Decode one right-hand side: a quoted string, `0x`-prefixed hex, or
/// plain decimal.
fn dump_number(v: &str) -> Option<u64> {
    if let Some(hex) = v.strip_prefix("0x") {
        u64::from_str_radix(hex, 16).ok()
    } else {
        v.parse::<u64>().ok()
    }
}

fn parse_rootdump(text: &str) -> RootDump {
    let mut dump = RootDump::default();
    for line in text.lines() {
        let line = line.trim();
        // The generator writes the root inode number on its own line, so
        // the dump names the inode it describes rather than relying on
        // the reader to have picked the same one.
        if let Some(rest) = line.strip_prefix("ROOTINO ") {
            dump.rootino = dump_number(rest.trim());
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());

        if k.ends_with(".hdr.count") {
            dump.count = dump_number(v);
        } else if k.ends_with(".hdr.i8count") {
            dump.i8count = dump_number(v);
        } else if k.contains(".hdr.parent") {
            dump.parent = dump_number(v);
        } else if let Some(rest) = k.split_once(".list[").map(|(_, r)| r) {
            let Some((index, field)) = rest.split_once("].") else {
                continue;
            };
            let Ok(index) = index.parse::<usize>() else {
                continue;
            };
            let e = dump.entries.entry(index).or_default();
            match field {
                "namelen" => e.namelen = dump_number(v),
                "offset" => e.offset = dump_number(v),
                "name" => e.name = Some(v.trim_matches('"').to_string()),
                // `.i4` or `.i8`, depending on the directory's width.
                f if f.starts_with("inumber") => e.ino = dump_number(v),
                "filetype" => e.ftype = dump_number(v),
                _ => {}
            }
        }
    }
    dump
}

/// The file type byte as `xfs_db` reports it, mapped to what this driver
/// returns. `XFS_DIR3_FT_UNKNOWN` and the overlay whiteout type have no
/// representable counterpart.
fn oracle_ftype(raw: u64) -> Option<FileType> {
    match raw {
        1 => Some(FileType::Regular),
        2 => Some(FileType::Directory),
        3 => Some(FileType::CharDevice),
        4 => Some(FileType::BlockDevice),
        5 => Some(FileType::Fifo),
        6 => Some(FileType::Socket),
        7 => Some(FileType::Symlink),
        _ => None,
    }
}

/// Compare one field, naming it and both readings on mismatch.
fn expect<T: PartialEq + std::fmt::Debug>(
    theirs: Option<T>,
    ours: T,
    field: &str,
    label: &str,
    checked: &mut usize,
) {
    let Some(theirs) = theirs else {
        // Field not reported by this xfsprogs version. Say so rather
        // than passing silently — an unnoticed skip is a hole in the
        // gate.
        eprintln!("  {label}: xfs_db did not report `{field}` — not compared");
        return;
    };
    assert_eq!(
        ours, theirs,
        "{label}: field `{field}` — this driver says {ours:?}, xfs_db says {theirs:?}"
    );
    *checked += 1;
}

/// Every field of every short-form root directory must match what the
/// reference debugger reports for it.
///
/// This is the test that pins the short-form layout down. The entry
/// widths depend on the name length, on whether the filesystem stores a
/// file type byte, and on the header's `i8count`; the parser insists on
/// consuming exactly `di_size` bytes, so a single wrong assumption about
/// any of those makes the whole directory fail to parse rather than
/// produce subtly wrong names.
#[test]
fn root_directory_agrees_with_xfs_db() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures in .vm-share — run ./scripts/vm-build-fixtures.sh; skipping");
        return;
    }

    let mut examined = 0usize;
    let mut total_fields = 0usize;
    for (label, img, dump_path) in &images {
        let Some(dump_path) = dump_path else {
            continue;
        };
        let bytes = std::fs::read(img).expect("read image");
        let (sb, inode, fork) = root_dir_fork(&bytes, label);
        let dump = parse_rootdump(&std::fs::read_to_string(dump_path).expect("read rootdump"));

        expect(dump.rootino, sb.rootino, "rootino", label, &mut 0);
        assert_eq!(
            inode.format,
            Format::Local,
            "{label}: this comparison only covers short-form directories"
        );

        let parsed = dir::read_short_form(&inode, &bytes[fork], &sb)
            .unwrap_or_else(|e| panic!("{label}: failed to parse the root directory: {e}"));

        let mut checked = 0usize;
        expect(
            dump.count,
            parsed.entries.len() as u64,
            "hdr.count",
            label,
            &mut checked,
        );
        expect(
            dump.i8count,
            u64::from(parsed.i8count),
            "hdr.i8count",
            label,
            &mut checked,
        );
        expect(
            dump.parent,
            parsed.parent_ino,
            "hdr.parent",
            label,
            &mut checked,
        );

        assert_eq!(
            dump.entries.len(),
            parsed.entries.len(),
            "{label}: xfs_db lists {} entries, this driver found {}",
            dump.entries.len(),
            parsed.entries.len()
        );

        for (i, ours) in parsed.entries.iter().enumerate() {
            let theirs = dump
                .entries
                .get(&i)
                .unwrap_or_else(|| panic!("{label}: xfs_db has no entry {i}"));
            let at = format!("{label} entry[{i}]");
            expect(
                theirs.name.clone(),
                String::from_utf8_lossy(&ours.name).into_owned(),
                "name",
                &at,
                &mut checked,
            );
            expect(
                theirs.namelen,
                ours.name.len() as u64,
                "namelen",
                &at,
                &mut checked,
            );
            expect(
                theirs.offset,
                u64::from(ours.offset),
                "offset",
                &at,
                &mut checked,
            );
            expect(theirs.ino, ours.ino, "inumber", &at, &mut checked);
            if let Some(raw) = theirs.ftype {
                expect(
                    Some(oracle_ftype(raw)),
                    ours.ftype,
                    "filetype",
                    &at,
                    &mut checked,
                );
            }

            // Every name must resolve to an inode that actually exists
            // and whose type agrees with the entry's file type byte.
            // This is the cross-check the file type exists for, and it
            // fails loudly if the byte were read from the wrong place.
            let off = inode_offset(&sb, ours.ino);
            let target = Inode::parse(&bytes[off..], &sb, ours.ino).unwrap_or_else(|e| {
                panic!(
                    "{at}: entry names inode {} which does not parse: {e}",
                    ours.ino
                )
            });
            if let Some(ft) = ours.ftype {
                assert_eq!(
                    Some(ft),
                    target.file_type(),
                    "{at}: entry says {ft:?} but inode {} is {:?}",
                    ours.ino,
                    target.file_type()
                );
            }
        }

        assert!(
            checked >= 10,
            "{label}: only {checked} directory fields could be compared — the oracle \
             dump is not providing enough coverage to call this validated"
        );
        eprintln!(
            "  {label}: root directory, {} entries, {checked} fields agree with xfs_db",
            parsed.entries.len()
        );
        examined += 1;
        total_fields += checked;
    }

    assert!(
        examined > 0,
        "no .rootdump fixtures found — the directory parser is unvalidated"
    );
    eprintln!("{examined} root directories, {total_fields} field comparisons against xfs_db");
}

/// The root directory of every image must parse, dump or no dump.
#[test]
fn root_directory_parses_on_real_images() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures in .vm-share — skipping");
        return;
    }

    let mut examined = 0usize;
    let mut spilled = 0usize;
    for (label, img, _dump_path) in &images {
        let bytes = std::fs::read(img).expect("read image");
        let (sb, inode, fork) = root_dir_fork(&bytes, label);

        assert!(
            inode.is_dir(),
            "{label}: the root inode is not a directory (mode {:#o})",
            inode.mode
        );

        // A directory with enough in it has outgrown its inode, and the
        // assertions below are about the short form specifically. Rather
        // than skip those images — which would quietly drop the largest
        // directories from the only test that reads every fixture's root
        // — they go through the driver's real lookup path, which is the
        // one a caller would use and the one that has to follow the
        // extent map to find the blocks.
        //
        // A fixture's root is not always the big directory: the create
        // fixtures keep their filler in a subdirectory precisely so the
        // root stays short form, so the descent below is what reaches
        // the spilled one there.
        if inode.format != Format::Local {
            let fs =
                fs_xfs::Filesystem::mount(Arc::new(FileDevice::open(img).expect("open the image")))
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
            let (root, raw) = fs
                .read_inode_raw(sb.rootino)
                .unwrap_or_else(|e| panic!("{label}: {e}"));
            let entries = fs
                .read_dir(&root, &raw)
                .unwrap_or_else(|e| panic!("{label}: failed to read the root directory: {e}"));

            // `.` and `..` become real entries once a directory leaves
            // the inode, where the short form keeps the parent in its
            // header and materialises neither. `read_dir` filters them
            // so a caller does not see a listing change shape purely
            // because the directory grew — and this is the only test
            // that reaches a directory where there is something to
            // filter.
            assert!(
                !entries.iter().any(|e| e.name == b"." || e.name == b".."),
                "{label}: `.` and `..` should not appear in a listing, so that a \
                 directory past the short form lists the same way as one inside it"
            );
            assert!(
                !entries.is_empty(),
                "{label}: a root that has outgrown its inode should hold entries"
            );

            // Every entry must name an inode that can actually be read.
            // That is what says the extent map was followed to real
            // directory blocks rather than to something that merely
            // parsed — a wrong block would give names and inode numbers
            // that go nowhere.
            for entry in entries.iter().take(8) {
                fs.read_inode(entry.ino).unwrap_or_else(|e| {
                    panic!(
                        "{label}: the root lists {:?} as inode {}, which does not read: {e}",
                        String::from_utf8_lossy(&entry.name),
                        entry.ino
                    )
                });
            }

            eprintln!(
                "  {label}: v{}, ftype {}, {:?} root, {} bytes, {} entries",
                sb.version(),
                if sb.has_ftype() { "on" } else { "off/v4" },
                inode.format,
                inode.size,
                entries.len()
            );
            examined += 1;
            spilled += 1;
            continue;
        }

        // The root is short form. One of the directories it names may
        // not be — and if none is ever checked, the extent-map path goes
        // untested however many images there are.
        spilled += check_subdirectories(img, label);

        let parsed = dir::read_short_form(&inode, &bytes[fork], &sb)
            .unwrap_or_else(|e| panic!("{label}: failed to parse the root directory: {e}"));

        // `..` of the root is the root, on every XFS filesystem.
        assert_eq!(
            parsed.parent_ino, sb.rootino,
            "{label}: the root directory's parent is {} rather than itself ({})",
            parsed.parent_ino, sb.rootino
        );

        // A root nothing was ever created in is the header alone: two
        // counts and a 32-bit parent inode number, which `xfs_db`
        // reports as core.size = 6.
        //
        // Which fixtures are empty is a property of how each was built,
        // not something this test can infer — it used to key off whether
        // an `xfs_db` dump existed alongside, and started failing the
        // moment a populated fixture arrived without one. So the size is
        // checked against what the directory actually holds.
        const EMPTY_SHORT_FORM_SIZE: u64 = 6;
        if parsed.entries.is_empty() {
            assert_eq!(
                inode.size, EMPTY_SHORT_FORM_SIZE,
                "{label}: a short-form root with no entries should be exactly \
                 {EMPTY_SHORT_FORM_SIZE} bytes"
            );
        } else {
            assert!(
                inode.size > EMPTY_SHORT_FORM_SIZE,
                "{label}: a root holding {} entries reports {} bytes, which is not \
                 even the empty header",
                parsed.entries.len(),
                inode.size
            );
        }

        eprintln!(
            "  {label}: v{}, ftype {}, {} bytes, {} entries",
            sb.version(),
            if sb.has_ftype() { "on" } else { "off/v4" },
            inode.size,
            parsed.entries.len()
        );
        examined += 1;
    }
    eprintln!("{examined} real root directories parsed ({spilled} of them past the short form)");
    assert!(
        spilled > 0,
        "no fixture holds a directory past the short form, so the path that follows an \
         extent map to find a directory's blocks is never taken here — build the create \
         fixtures, whose `fill` directory holds enough entries to have spilled"
    );
}

/// Check any directory the root names that has outgrown its inode.
///
/// Returns how many were found, so the caller can tell whether the
/// extent-map path was exercised at all.
///
/// The checks are the same as the root's: a listing that omits `.` and
/// `..`, and entries that name inodes which actually read. What differs
/// is only how the directory was reached.
fn check_subdirectories(img: &Path, label: &str) -> usize {
    // A fixture with work still in its log cannot be mounted, and that
    // is the driver behaving correctly rather than a failure here. Any
    // other error is real and is raised.
    let fs =
        match fs_xfs::Filesystem::mount(Arc::new(FileDevice::open(img).expect("open the image"))) {
            Ok(fs) => fs,
            Err(fs_xfs::Error::DirtyLog) => return 0,
            Err(e) => panic!("{label}: {e}"),
        };
    let sb = fs.superblock().clone();

    let (root, root_raw) = fs
        .read_inode_raw(sb.rootino)
        .unwrap_or_else(|e| panic!("{label}: {e}"));
    let entries = fs
        .read_dir(&root, &root_raw)
        .unwrap_or_else(|e| panic!("{label}: {e}"));

    let mut found = 0usize;
    for entry in &entries {
        let Ok((child, child_raw)) = fs.read_inode_raw(entry.ino) else {
            continue;
        };
        if !child.is_dir() || child.format == Format::Local {
            continue;
        }
        let listing = fs.read_dir(&child, &child_raw).unwrap_or_else(|e| {
            panic!(
                "{label}: failed to read {:?}, which has outgrown its inode: {e}",
                String::from_utf8_lossy(&entry.name)
            )
        });
        assert!(
            !listing.iter().any(|e| e.name == b"." || e.name == b".."),
            "{label}: `.` and `..` should not appear in a listing"
        );
        assert!(
            !listing.is_empty(),
            "{label}: a directory past the short form should hold entries"
        );
        for e in listing.iter().take(8) {
            fs.read_inode(e.ino).unwrap_or_else(|why| {
                panic!(
                    "{label}: {:?} lists {:?} as inode {}, which does not read: {why}",
                    String::from_utf8_lossy(&entry.name),
                    String::from_utf8_lossy(&e.name),
                    e.ino
                )
            });
        }
        eprintln!(
            "  {label}: {:?} is {:?}, {} entries",
            String::from_utf8_lossy(&entry.name),
            child.format,
            listing.len()
        );
        found += 1;
    }
    found
}

/// A directory data block found by scanning, and what it held.
struct FoundBlock {
    owner: u64,
    entries: Vec<DirEntry>,
}

/// Find, verify and parse every block-, leaf- and node-form directory
/// block in the real images.
///
/// Directories large enough to spill out of their inode live in blocks
/// the extent map points at, and this module owns no extent map — so the
/// blocks are found by scanning the image for their magics instead. That
/// is not a shortcut around the real lookup path: it reaches genuine
/// metadata a real kernel wrote, which is the whole point.
///
/// The v5 self-describing header is what makes the scan safe. A data
/// block is claimed by a four-byte magic, which no plausible amount of
/// file data will produce by accident, so every one found is required to
/// pass its CRC32C, carry the filesystem's UUID, and record the address
/// it was found at.
///
/// Leaf and node blocks are claimed by a **two-byte** magic, and at one
/// chance in 65536 per block a 500 MB image contains several by luck —
/// this test found one on its first run. Those candidates are therefore
/// filtered on the filesystem UUID before anything is asserted about
/// them, and the CRC, the recorded address and the owning inode are
/// still checked afterwards. Using the UUID as the filter costs the
/// ability to assert it, which is the cheapest of the four checks and
/// the only one already covered by the data blocks.
///
/// The internal log is excluded, because it holds images of metadata
/// blocks whose recorded address is where they belong rather than where
/// they are sitting.
#[test]
fn directory_blocks_in_real_images_parse_and_verify() {
    let images = images();
    if images.is_empty() {
        eprintln!("no fixtures in .vm-share — skipping");
        return;
    }

    let mut total_blocks = 0usize;
    let mut total_entries = 0usize;
    for (label, img, dump_path) in &images {
        // Only the populated fixtures have directories big enough to
        // reach a block, and reading every image is slow.
        if dump_path.is_none() {
            continue;
        }
        let bytes = std::fs::read(img).expect("read image");
        let sb = Superblock::parse(&bytes).expect("parse superblock");
        if !sb.is_v5() {
            // Without the self-describing header a scan cannot tell a
            // real directory block from four coincidental bytes.
            eprintln!("  {label}: v4, not scannable without identity fields — skipping");
            continue;
        }

        let blocksize = sb.blocksize as usize;
        let dirblocksize = sb.dirblocksize() as usize;
        let log = if sb.has_internal_log() {
            let start = fsb_to_byte(&sb, sb.logstart);
            start..start + u64::from(sb.logblocks) * u64::from(sb.blocksize)
        } else {
            0..0
        };

        let mut found: Vec<FoundBlock> = Vec::new();
        let mut leaves = 0usize;
        let mut nodes = 0usize;
        let mut blocks = 0usize;
        let mut datas = 0usize;
        let mut coincidences = 0usize;

        let mut off = 0usize;
        while off + dirblocksize <= bytes.len() {
            let at = off;
            off += blocksize;
            if log.contains(&(at as u64)) {
                continue;
            }
            let block = &bytes[at..at + dirblocksize];
            let daddr = (at / 512) as u64;

            let magic32 = u32::from_be_bytes(block[0..4].try_into().unwrap());
            let is_block = magic32 == dir::XFS_DIR3_BLOCK_MAGIC;
            let is_data = magic32 == dir::XFS_DIR3_DATA_MAGIC;
            if is_block || is_data {
                let owner = be64(block, dir::offsets::dir3_blk::OWNER);
                dir::verify_data_block(block, &sb, daddr, owner).unwrap_or_else(|e| {
                    panic!("{label}: directory block at byte {at} failed verification: {e}")
                });
                let entries = dir::parse_data_block(block, &sb).unwrap_or_else(|e| {
                    panic!("{label}: directory block at byte {at} failed to parse: {e}")
                });
                if is_block {
                    blocks += 1;
                    // A single-block directory also carries its hash
                    // index, and the two must agree on how many live
                    // names there are.
                    let bd = dir::parse_block_form(block, &sb).unwrap();
                    let live = bd.index.iter().filter(|e| !e.is_stale()).count();
                    assert_eq!(
                        live,
                        bd.entries.len(),
                        "{label}: block-form directory at {at} has {} entries but {live} \
                         live index records",
                        bd.entries.len()
                    );
                } else {
                    datas += 1;
                }
                total_entries += entries.len();
                found.push(FoundBlock { owner, entries });
                continue;
            }

            let magic16 = u16::from_be_bytes(
                block[dir::offsets::da_blk::MAGIC..dir::offsets::da_blk::MAGIC + 2]
                    .try_into()
                    .unwrap(),
            );
            if matches!(
                magic16,
                dir::XFS_DIR3_LEAF1_MAGIC | dir::XFS_DIR3_LEAFN_MAGIC | dir::XFS_DA3_NODE_MAGIC
            ) {
                // Two bytes of magic is not enough to tell a real index
                // block from a coincidence in file data; the UUID is.
                let uuid = &block[dir::offsets::da_blk::UUID..dir::offsets::da_blk::UUID + 16];
                if uuid != sb.meta_uuid {
                    coincidences += 1;
                    continue;
                }
            }
            match magic16 {
                dir::XFS_DIR3_LEAF1_MAGIC | dir::XFS_DIR3_LEAFN_MAGIC => {
                    let owner = be64(block, dir::offsets::da_blk::OWNER);
                    dir::verify_da_block(block, &sb, daddr, owner).unwrap_or_else(|e| {
                        panic!("{label}: leaf block at byte {at} failed verification: {e}")
                    });
                    let leaf = dir::parse_leaf(block, &sb).unwrap_or_else(|e| {
                        panic!("{label}: leaf block at byte {at} failed to parse: {e}")
                    });
                    assert_eq!(
                        leaf.entries.len(),
                        usize::from(leaf.count),
                        "{label}: leaf block at {at} disagrees with its own count"
                    );
                    leaves += 1;
                }
                dir::XFS_DA3_NODE_MAGIC => {
                    let owner = be64(block, dir::offsets::da_blk::OWNER);
                    dir::verify_da_block(block, &sb, daddr, owner).unwrap_or_else(|e| {
                        panic!("{label}: node block at byte {at} failed verification: {e}")
                    });
                    let node = dir::parse_node(block, &sb).unwrap_or_else(|e| {
                        panic!("{label}: node block at byte {at} failed to parse: {e}")
                    });
                    assert!(node.level >= 1, "{label}: node block at {at} is at level 0");
                    nodes += 1;
                }
                _ => {}
            }
        }

        // Everything the scan found must hang together with the inodes
        // it names.
        for fb in &found {
            let off = inode_offset(&sb, fb.owner);
            let owner_inode = Inode::parse(&bytes[off..], &sb, fb.owner).unwrap_or_else(|e| {
                panic!(
                    "{label}: a directory block claims owner {} which does not parse: {e}",
                    fb.owner
                )
            });
            assert!(
                owner_inode.is_dir(),
                "{label}: a directory block is owned by inode {}, which is not a directory",
                fb.owner
            );
            // A block-form or data-form directory stores `.` and `..` as
            // real entries, and `.` names the block's own owner. Those
            // are two independent fields -- the header's owner at one
            // offset and the entry's inode number at another -- so their
            // agreement is a real check on both.
            if let Some(dot) = fb.entries.iter().find(|e| e.name == b".") {
                assert_eq!(
                    dot.ino, fb.owner,
                    "{label}: `.` names inode {} but the block's owner is {}",
                    dot.ino, fb.owner
                );
                assert_eq!(
                    dot.ftype,
                    Some(FileType::Directory),
                    "{label}: `.` is not typed as a directory"
                );
            }
            for e in &fb.entries {
                assert!(!e.name.is_empty(), "{label}: an entry has an empty name");
                assert!(
                    !e.name.contains(&b'/') && !e.name.contains(&0),
                    "{label}: entry name {:?} holds a byte no name may contain",
                    String::from_utf8_lossy(&e.name)
                );
            }
        }

        eprintln!(
            "  {label}: {blocks} block-form, {datas} data, {leaves} leaf, {nodes} node \
             directory blocks verified and parsed ({coincidences} two-byte magic \
             coincidences filtered out by UUID)"
        );
        total_blocks += blocks + datas + leaves + nodes;
    }

    assert!(
        total_blocks > 0,
        "no directory blocks were found in any image. Either the fixtures hold no \
         directory large enough to leave its inode, or the scan is looking in the \
         wrong place — both mean the block, leaf and node paths are unvalidated"
    );
    eprintln!("{total_blocks} directory blocks verified, {total_entries} entries parsed");
}

/// A directory read out of the wrong inode must be rejected.
///
/// The root inode's data fork is handed to the parser under a claim that
/// it belongs to a regular file, and under a claim that its fork is in
/// extent format. Both are refused, on real metadata rather than on a
/// fixture built to be refused.
#[test]
fn real_root_directory_rejects_wrong_claims() {
    let images = images();
    let Some((label, img, _)) = images.first() else {
        eprintln!("no fixtures in .vm-share — skipping");
        return;
    };

    let bytes = std::fs::read(img).expect("read image");
    let (sb, inode, range) = root_dir_fork(&bytes, label);
    let fork = &bytes[range];

    let mut not_a_dir = inode.clone();
    not_a_dir.mode = 0o100_644;
    assert!(
        matches!(
            dir::read_short_form(&not_a_dir, fork, &sb),
            Err(fs_xfs::Error::NotADirectory)
        ),
        "{label}: a regular file's fork was read as a directory"
    );

    let mut wrong_format = inode.clone();
    wrong_format.format = Format::Extents;
    assert!(
        dir::read_short_form(&wrong_format, fork, &sb).is_err(),
        "{label}: an extent-format fork was read as short form"
    );

    // Truncating the fork by a byte must be rejected too: the parser is
    // measuring the entries against the declared size, not tolerating
    // whatever it happens to find.
    let short = &fork[..fork.len() - 1];
    let mut truncated = inode.clone();
    truncated.size = short.len() as u64 + 1;
    assert!(
        dir::read_short_form(&truncated, short, &sb).is_err(),
        "{label}: a truncated short-form directory was accepted"
    );
}

/// The file-type feature must be read from the right place on both
/// on-disk versions.
///
/// Regression test for a real bug, now fixed.
///
/// `Superblock::has_ftype` used to test only the v5 incompatible feature
/// bit, but a **v4** filesystem advertises the same feature in
/// `sb_features2`. The `xfs-nocrc` fixture is v4 with
/// `features_incompat = 0` and `features2 = 0x28a`, bit `0x200` of which
/// is the file-type flag. A directory parser trusting the old
/// `has_ftype()` would read every one of its entries one byte short, and
/// every inode number would come out shifted.
///
/// This fixture is the only one in the matrix that can catch it, so the
/// test pins the fixture's own properties too — if `xfs-nocrc` ever
/// stops being a v4 filesystem carrying the flag, the coverage is gone
/// and the assertions below say so rather than passing vacuously.
#[test]
fn v4_fixture_advertises_ftype_outside_the_incompat_mask() {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let img = share.join("xfs-nocrc.img");
    if !img.exists() {
        eprintln!("no xfs-nocrc.img — skipping");
        return;
    }
    let bytes = std::fs::read(&img).expect("read image");
    let sb = Superblock::parse(&bytes).expect("parse superblock");

    assert_eq!(sb.version(), 4, "xfs-nocrc is meant to be a v4 filesystem");
    assert_eq!(
        sb.features_incompat, 0,
        "a v4 filesystem has no incompatible feature mask"
    );
    assert!(
        sb.has_ftype(),
        "has_ftype() must see the file-type flag a v4 filesystem carries in \
         sb_features2, not only the v5 incompatible bit"
    );
    assert_ne!(
        sb.features2 & 0x200,
        0,
        "xfs-nocrc is expected to carry the file-type flag in sb_features2"
    );
    assert_ne!(
        sb.versionnum & 0x8000,
        0,
        "sb_features2 is only meaningful when the MOREBITS flag is set"
    );
}

/// Renaming refuses a v4 filesystem, as every other journalled write
/// does.
///
/// `rename_in_directory` was the one journalled entry point with no v5
/// gate. Creating, removing, truncating and writing all refuse a v4
/// image by name; renaming would have gone ahead and stamped v5
/// self-describing headers — CRCs and owner fields — onto a filesystem
/// with nowhere to put them.
///
/// `xfs-nocrc` is the only v4 fixture in the matrix, so it is the only
/// thing that can catch this. The test pins its version too, so that if
/// the fixture ever stops being v4 the assertion says so rather than
/// passing for the wrong reason.
#[test]
fn renaming_refuses_a_v4_filesystem() {
    use fs_core::BlockDevice;
    use std::sync::Arc;

    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let img = share.join("xfs-nocrc.img");
    if !img.exists() {
        eprintln!("no xfs-nocrc.img — skipping");
        return;
    }

    // Work on a copy: mount_rw takes a writable device and this must not
    // disturb the shared fixture.
    let tmp = std::env::temp_dir().join("xfs-nocrc-rename-gate.img");
    std::fs::copy(&img, &tmp).expect("copy fixture");

    let bytes = std::fs::read(&tmp).expect("read image");
    let sb = Superblock::parse(&bytes).expect("parse superblock");
    assert_eq!(
        sb.version(),
        4,
        "this test needs a v4 fixture to mean anything"
    );

    let dev: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_rw(&tmp).expect("open rw"));
    let fs = fs_xfs::fs::Filesystem::mount_rw(dev).expect("mount the v4 image");

    let err = fs
        .rename_in_directory(sb.rootino, b"anything", b"anything-else")
        .expect_err("renaming must refuse a v4 filesystem");
    let msg = format!("{err}");
    assert!(
        msg.contains("v5 metadata"),
        "the refusal should say why, like the other write paths do: {msg}"
    );

    let _ = std::fs::remove_file(&tmp);
}
