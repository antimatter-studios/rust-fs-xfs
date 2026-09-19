//! Seeded random sequences of write operations, each replayed by the kernel,
//! leave volumes `xfs_repair -n` accepts.
//!
//! Each replay oracle pins one shape. This one looks for shapes nobody wrote a
//! test for. It mixes every public mutation over a handful of names in the
//! root and in a subdirectory:
//! - logged: create, mkdir, unlink, rename, truncate to zero, first write;
//! - in place: write, set attributes, truncate.
//!
//! A mount writes at most one checkpoint, so every operation gets its own
//! mount, and the kernel then mounts and unmounts the image, which replays the
//! record. `xfs_repair -n` runs after every step. A refused operation is fine;
//! a record the kernel cannot replay, or a volume xfs_repair rejects, is not.
//! The sequence depends only on the seed, so a failure prints the operations
//! so far, and the same seed reproduces it.
//!
//! A run of 480 steps over six seeds, and 60 steps over each of 1 KiB blocks,
//! rmapbt, reflink off and 8 KiB directory blocks, found nothing on the day it
//! was written; it is here so the next change to a write path meets it.
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, share};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::write::AttrChange;
use fs_xfs::Filesystem;
use std::sync::Arc;

const STEPS: u64 = 40;
const SEEDS: [u64; 2] = [1, 2];

/// xorshift64: small, deterministic, and the same on every platform.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

/// One operation on its own mount, chosen by `rng`, described for the
/// failure message.
fn operate(image: &str, rng: &mut Rng, step: u64) -> String {
    const NAMES: [&[u8]; 8] = [
        b"a",
        b"b",
        b"c",
        b"d",
        b"e",
        b"long_name_number_one",
        b"x",
        b"y",
    ];
    let dev = Arc::new(FileDevice::open_rw(image).unwrap());
    let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>)
        .unwrap_or_else(|e| panic!("step {step}: mount_rw after a replay failed: {e:?}"));
    let root = fs.lookup_path("/").unwrap().ino;
    let in_subdir = rng.below(3) == 0;
    let parent = if in_subdir {
        fs.lookup_path("/d").map(|i| i.ino).unwrap_or(root)
    } else {
        root
    };
    let n = NAMES[rng.below(NAMES.len() as u64) as usize];
    let m = NAMES[rng.below(NAMES.len() as u64) as usize];
    let path = format!(
        "{}/{}",
        if parent == root { "" } else { "/d" },
        String::from_utf8_lossy(n)
    );
    let found = fs.lookup_path(&path).ok();
    let (what, result): (String, Result<(), fs_xfs::Error>) = match rng.below(10) {
        0 | 1 => (
            format!("create {path}"),
            fs.create_file(parent, n, 0o100644).map(|_| ()),
        ),
        2 => (
            format!("mkdir {path}"),
            fs.create_directory(parent, n, 0o40755).map(|_| ()),
        ),
        3 => (
            format!("unlink {path}"),
            fs.unlink_file(parent, n).map(|_| ()),
        ),
        4 => (
            format!("rename {path} -> {}", String::from_utf8_lossy(m)),
            fs.rename_in_directory(parent, n, m).map(|_| ()),
        ),
        5 => match found {
            Some(i) => (
                format!("truncate_to_zero {path}"),
                fs.truncate_to_zero(i.ino).map(|_| ()),
            ),
            None => (format!("truncate_to_zero {path} (absent)"), Ok(())),
        },
        6 => match found {
            Some(i) => {
                let len = 1 + rng.below(40_000) as usize;
                (
                    format!("write_into_empty_file {path} {len}"),
                    fs.write_into_empty_file(i.ino, &vec![7; len]).map(|_| ()),
                )
            }
            None => (format!("write_into_empty_file {path} (absent)"), Ok(())),
        },
        7 => match found {
            Some(i) => {
                let (inode, raw) = fs.read_inode_raw(i.ino).unwrap();
                let off = rng.below(inode.size.max(1));
                (
                    format!("write_at {path} {off}"),
                    fs.write_at(&inode, &raw, off, b"hello").map(|_| ()),
                )
            }
            None => (format!("write_at {path} (absent)"), Ok(())),
        },
        8 => match found {
            Some(i) => {
                let (inode, _) = fs.read_inode_raw(i.ino).unwrap();
                let change = AttrChange {
                    permissions: Some(rng.below(0o7777) as u16),
                    uid: Some(rng.below(2000) as u32),
                    ..Default::default()
                };
                (
                    format!("set_attributes {path}"),
                    fs.set_attributes(&inode, &change),
                )
            }
            None => (format!("set_attributes {path} (absent)"), Ok(())),
        },
        _ => match found {
            Some(i) => {
                let (inode, _) = fs.read_inode_raw(i.ino).unwrap();
                let size = rng.below(inode.size + 1);
                (
                    format!("truncate {path} {size}"),
                    fs.truncate(&inode, size, None),
                )
            }
            None => (format!("truncate {path} (absent)"), Ok(())),
        },
    };
    format!("{step}: {what} -> {result:?}")
}

#[test]
fn random_operation_sequences_replay_to_volumes_xfs_repair_accepts() {
    let dir = share().join("random-ops");
    std::fs::create_dir_all(&dir).unwrap();
    for seed in SEEDS {
        let name = format!("random-ops/replay-{seed}-{}.img", std::process::id());
        let image = share().join(&name);
        std::fs::File::create(&image)
            .and_then(|f| f.set_len(320 * 1024 * 1024))
            .unwrap();
        // Formatted where the kernel runs, so a host without xfsprogs, such
        // as the macOS runner, skips with the kernel rather than failing
        // here.
        let Some(mkfs) = kernel_run(&format!(
            "mkfs.xfs -q -f /share/{name} && echo MKFS_OK || echo MKFS_FAILED; echo DONE"
        )) else {
            let _ = std::fs::remove_file(&image);
            eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
            return;
        };
        assert!(mkfs.contains("MKFS_OK"), "mkfs.xfs failed:\n{mkfs}");
        let image_path = image.to_str().unwrap().to_string();

        // Mount to replay, then judge. The mount and unmount are what apply
        // the record the step wrote.
        let script = format!(
            r#"
            m=$(mktemp -d)
            if mount -o loop,nouuid /share/{name} "$m"; then
                # RETRIED ONCE. A busy unmount under a loaded runner is
                # ordinary and clears in a moment; one that does not is the
                # failure worth reporting, because the kernel writes the
                # summary counters at unmount and nothing else does.
                if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
                echo MOUNTED
            else
                echo MOUNT_FAILED
                dmesg | tail -12
            fi
            rmdir "$m"
            echo "REPAIR_BEGIN"
            xfs_repair -n /share/{name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
            echo "REPAIR_END"
            echo DONE
            "#
        );

        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut log = Vec::new();
        for step in 0..STEPS {
            log.push(operate(&image_path, &mut rng, step));
            let Some(out) = kernel_run(&script) else {
                let _ = std::fs::remove_file(&image);
                eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
                return;
            };
            assert!(
                out.contains("MOUNTED"),
                "[seed {seed}] after step {step} the kernel refused the volume:\n{out}\n\
                 operations:\n{}",
                log.join("\n")
            );
            repair::assert_agreed(
                &out,
                &format!(
                    "[seed {seed}] after step {step}, with these operations:\n{}",
                    log.join("\n")
                ),
            );
        }
        let _ = std::fs::remove_file(&image);
    }
}
