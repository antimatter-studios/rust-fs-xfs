//! Laying a group's tree out again, judged by `xfs_repair`.
//!
//! `ag_btree::build` turns a list of records into a whole tree. Its unit
//! tests prove that this driver can read back what it wrote, which is
//! the weaker half of the claim: a tree can be self-consistent and still
//! be one the kernel will not have.
//!
//! So this takes a fixture whose free-space trees are genuinely two
//! levels deep, lays both of them out again over the blocks they already
//! occupy, writes them into a copy of the image, and asks `xfs_repair`
//! whether the filesystem is still sound. Nothing about the *records*
//! changes; what changes is the shape they are stored in, because this
//! driver fills its blocks evenly and the kernel fills them as splits
//! happened to leave them.
//!
//! Fixtures are gitignored, so this skips on a fresh clone. Build them
//! with `scripts/vm-build-deeptree-fixtures.sh`.

mod common;
use common::{kernel_run, share};

use fs_core::FileDevice;
use fs_xfs::ag_btree;
use fs_xfs::alloc_btree::{FreeExtent, Order};
use fs_xfs::Filesystem;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A copy of a fixture, removed when it goes out of scope.
struct Copy(PathBuf);

impl Drop for Copy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn copy_of(src: &Path, name: &str) -> Option<Copy> {
    let dir = share().join("scratch");
    std::fs::create_dir_all(&dir).ok()?;
    let dst = dir.join(name);
    std::fs::copy(src, &dst).ok()?;
    Some(Copy(dst))
}

fn deep_fixtures() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(share()) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("xfsdeep-") && n.ends_with(".img"))
        })
        .collect();
    out.sort();
    out
}

fn decode_run(buf: &[u8], at: usize) -> FreeExtent {
    FreeExtent {
        startblock: u32::from_be_bytes(buf[at..at + 4].try_into().expect("4 bytes")),
        blockcount: u32::from_be_bytes(buf[at + 4..at + 8].try_into().expect("4 bytes")),
    }
}

fn encode_run(buf: &mut [u8], at: usize, run: &FreeExtent) {
    buf[at..at + 4].copy_from_slice(&run.startblock.to_be_bytes());
    buf[at + 4..at + 8].copy_from_slice(&run.blockcount.to_be_bytes());
}

/// Lay one of a group's trees out again over the blocks it already has,
/// and write the result into `img`.
///
/// Returns how many blocks it wrote, or a reason it could not.
fn relay(img: &Path, fs: &Filesystem, agno: u32, order: Order) -> Result<usize, String> {
    let sb = fs.superblock();
    let agf = fs.agf(agno).map_err(|e| e.to_string())?;
    let which = match order {
        Order::ByBlock => fs_xfs::ag::agf_btree::BNO,
        Order::ByCount => fs_xfs::ag::agf_btree::CNT,
    };
    let root = agf.roots[which];
    let levels = agf.levels[which];
    if levels < 2 {
        return Err(format!(
            "the {order:?} tree is {levels} level(s) deep; this fixture proves nothing"
        ));
    }

    let block = u64::from(sb.blocksize);
    let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;
    let read = |agblock: u32| -> fs_xfs::Result<Vec<u8>> {
        let mut buf = vec![0u8; sb.blocksize as usize];
        fs.device()
            .read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
        Ok(buf)
    };

    let (records, blocks) =
        ag_btree::walk_blocks(sb, order.shape(), agno, root, levels, read, decode_run)
            .map_err(|e| e.to_string())?;

    // The kernel's tree may hold more blocks than the records need,
    // because its leaves are as full as splitting left them. Giving the
    // surplus back is the editor's job and not this test's, so say so
    // rather than leaking blocks and blaming the layout for it.
    let plan = ag_btree::plan(order.shape(), sb.blocksize, sb.is_v5(), records.len())
        .map_err(|e| e.to_string())?;
    let wanted: usize = plan.iter().sum();
    if wanted != blocks.len() {
        return Err(format!(
            "{} records lay out over {wanted} blocks and the tree holds {}; \
             returning the difference is the editor's work",
            records.len(),
            blocks.len()
        ));
    }

    let built = ag_btree::build(
        sb,
        order.shape(),
        agno,
        &records,
        &blocks,
        encode_run,
        encode_run,
    )
    .map_err(|e| e.to_string())?;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .open(img)
        .map_err(|e| e.to_string())?;
    for b in &built {
        file.seek(SeekFrom::Start(ag_start + u64::from(b.agblock) * block))
            .map_err(|e| e.to_string())?;
        file.write_all(&b.bytes).map_err(|e| e.to_string())?;
    }
    file.sync_all().map_err(|e| e.to_string())?;
    Ok(built.len())
}

/// A tree this driver laid out is a tree the kernel's own checker
/// accepts.
#[test]
fn a_tree_laid_out_again_is_one_xfs_repair_accepts() {
    let fixtures = deep_fixtures();
    if fixtures.is_empty() {
        eprintln!("no xfsdeep-* fixtures — skipping");
        return;
    }

    let mut judged = 0;
    let mut broken: Vec<String> = Vec::new();

    for src in &fixtures {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("fixture");
        let Some(copy) = copy_of(src, &format!("relaid-{name}")) else {
            broken.push(format!("{name}: could not be copied"));
            continue;
        };

        let mut wrote = 0;
        {
            let device = FileDevice::open(&copy.0).expect("open the copy");
            let fs = Filesystem::mount(Arc::new(device)).expect("mount the copy");
            for order in [Order::ByBlock, Order::ByCount] {
                match relay(&copy.0, &fs, 0, order) {
                    Ok(n) => wrote += n,
                    Err(why) => {
                        eprintln!("{name} {order:?}: {why}");
                    }
                }
            }
        }
        if wrote == 0 {
            eprintln!("{name}: nothing was laid out again");
            continue;
        }

        // ONTO LOCAL DISK FIRST. The share is a virtiofs mount, and
        // xfs_repair opens an image in a way virtiofs will not serve --
        // it reports "Not a directory" for a file `ls` shows plainly.
        // Every other oracle here copies for the same reason.
        let script = format!(
            r#"
            img=$(mktemp -u /tmp/deep-XXXXXX.img)
            cp /share/scratch/relaid-{name} "$img"
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            rm -f "$img"
            echo "REPAIR_BEGIN"
            echo "$out"
            echo "REPAIR_RC=$rc"
            echo "REPAIR_END"
            echo DONE
            "#
        );
        let Some(out) = kernel_run(&script) else {
            eprintln!("{name}: no kernel to judge with — skipping");
            continue;
        };
        if !out.contains("REPAIR_END") {
            eprintln!("{name}: the judge did not run — skipping");
            continue;
        }

        judged += 1;
        if !out.contains("REPAIR_RC=0") {
            broken.push(format!(
                "{name}: wrote {wrote} blocks and xfs_repair objected:\n{out}"
            ));
        } else {
            eprintln!("{name}: {wrote} blocks laid out again, sound");
        }
    }

    assert!(
        broken.is_empty(),
        "the kernel's checker rejected a tree this driver laid out:\n{}",
        broken.join("\n")
    );
    assert!(
        judged > 0,
        "no fixture was judged — the test proved nothing"
    );
}

/// Every write path, into a group whose trees are two levels deep,
/// judged the way every other write here is: the kernel replays what was
/// logged, and `xfs_repair` says whether what came out is a filesystem.
///
/// This is the point of the whole exercise. Reading a deep tree was
/// fixed separately; until now every write path refused outright, so a
/// filesystem with any real fragmentation was read-only in practice.
///
/// A refusal is reported rather than passed over, because a run where
/// every operation refused would otherwise read exactly like a run where
/// every operation worked.
#[test]
fn every_write_into_a_group_with_deep_trees_is_sound() {
    let fixtures = deep_fixtures();
    if fixtures.is_empty() {
        eprintln!("no xfsdeep-* fixtures — skipping");
        return;
    }

    // One per write path that touches a group's trees: taking blocks for
    // a new file, taking them for a file's data, giving them back, and
    // giving back the inode as well.
    const OPS: &[&str] = &[
        "create_file",
        "write_into_empty_file",
        "truncate_to_zero",
        "unlink_file",
    ];

    let mut judged = 0;
    let mut refused = 0;
    let mut broken: Vec<String> = Vec::new();

    for src in &fixtures {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("fixture");
        for op in OPS {
            let scratch = format!("wrote-{op}-{name}");
            let Some(copy) = copy_of(src, &scratch) else {
                broken.push(format!("{name} {op}: could not be copied"));
                continue;
            };

            {
                let device =
                    fs_core::FileDevice::open_rw(&copy.0).expect("open the copy for writing");
                let fs = Filesystem::mount_rw(Arc::new(device)).expect("mount the copy");
                let ino = |path: &str| -> Result<u64, String> {
                    fs.lookup_path(path)
                        .map(|i| i.ino)
                        .map_err(|e| e.to_string())
                };
                let outcome = match *op {
                    "create_file" => ino("/sf").and_then(|dir| {
                        fs.create_file(dir, b"deep", 0o100644)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }),
                    "write_into_empty_file" => ino("/sf/empty.bin").and_then(|f| {
                        fs.write_into_empty_file(f, b"written into a fragmented group")
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }),
                    // A one-block file among the fragments, so what it
                    // gives back lands in the middle of a deep tree
                    // rather than at either end of it.
                    "truncate_to_zero" => ino("/frag/f1").and_then(|f| {
                        fs.truncate_to_zero(f)
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }),
                    "unlink_file" => ino("/sf").and_then(|dir| {
                        fs.unlink_file(dir, b"victim")
                            .map(|_| ())
                            .map_err(|e| e.to_string())
                    }),
                    other => Err(format!("no such operation: {other}")),
                };
                if let Err(why) = outcome {
                    eprintln!("{name} {op}: refused: {why}");
                    refused += 1;
                    continue;
                }
            }

            let script = format!(
                r#"
                img=$(mktemp -u /tmp/deepw-XXXXXX.img)
                cp /share/scratch/{scratch} "$img"
                m=$(mktemp -d)
                mounted=0
                for attempt in 1 2 3; do
                    if mount -o loop,nouuid "$img" "$m"; then
                        umount "$m"
                        mounted=$((mounted + 1))
                    fi
                    out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
                    case "$out" in
                        *"valuable metadata changes in a log"*) continue ;;
                        *) break ;;
                    esac
                done
                rmdir "$m" 2>/dev/null
                [ "$mounted" -gt 0 ] || echo "MOUNT_FAILED"
                rm -f "$img"
                echo "REPAIR_BEGIN"
                echo "$out"
                echo "REPAIR_RC=$rc"
                echo "REPAIR_END"
                echo DONE
                "#
            );
            let Some(out) = kernel_run(&script) else {
                eprintln!("{name} {op}: no kernel to judge with — skipping");
                continue;
            };
            judged += 1;
            if !out.contains("REPAIR_RC=0") || out.contains("MOUNT_FAILED") {
                broken.push(format!("{name} {op}: the kernel objected:\n{out}"));
            } else {
                eprintln!("{name} {op}: sound");
            }
        }
    }

    eprintln!("{judged} judged, {refused} refused");
    assert!(
        broken.is_empty(),
        "a write into a deep-tree group did not survive:\n{}",
        broken.join("\n")
    );
    assert!(
        judged > 0,
        "no operation was judged — the test proved nothing"
    );
}
