//! A volume whose log was never replayed mounts, and reads as the kernel
//! reads it (#90).
//!
//! A crash, a panic or a yanked cable leaves the log holding committed
//! transactions the filesystem itself has not been given yet. Every
//! structure on disk may be a version the log was about to replace, so
//! this driver refused the volume outright — honest, and it meant the
//! ordinary state after a crash was one it could not open at all, where
//! the kernel simply replays and mounts.
//!
//! Replay here is applied **into memory**. The volume is not written to:
//! a read-only mount stays read-only, which is what recovering data from
//! a disk that must not be touched needs.
//!
//! The judgement is the kernel's. The same image is copied, the copy is
//! mounted by the kernel — which replays it, on disk, as it always does
//! — and what it then holds is listed. This driver reads the untouched
//! dirty image and has to say exactly the same thing: every name, every
//! size, every file's contents, every symlink's target.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volumes live, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "dirty_log_mount";
use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Files written and flushed before the crash: their data is on disk and
/// their metadata may or may not be.
const SETTLED: u32 = 20;

/// Directories made and then removed, so their blocks are freed and
/// handed out again inside the range being replayed.
const CHURN: u32 = 200;

/// Files written into the churn's blocks and fsynced, so what those
/// blocks hold afterwards is file data the log does not describe.
const REUSED: u32 = 40;

/// Files created afterwards, with nothing flushed behind them. These
/// exist in the log and nowhere else, so a driver that does not replay
/// cannot see one of them.
const UNFLUSHED: u32 = 50;

/// One line per name, in the shape the guest prints: `D <path>`,
/// `F <path> <size> <md5>`, `L <path> <target>`.
///
/// Ordered by path, which is what `find | sort` gives the guest — a
/// directory immediately before what is inside it, rather than every
/// directory first.
fn walk(fs: &Filesystem) -> Vec<String> {
    let mut out = Vec::new();
    let root = fs.root_inode().expect("the root inode");
    let raw = fs.read_inode_raw(root.ino).expect("the root inode raw").1;
    descend(fs, &root, &raw, ".", &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out.into_iter().map(|(_, line)| line).collect()
}

fn descend(
    fs: &Filesystem,
    dir: &fs_xfs::inode::Inode,
    raw: &[u8],
    at: &str,
    out: &mut Vec<(String, String)>,
) {
    for entry in fs.read_dir(dir, raw).expect("reading a directory") {
        let name = String::from_utf8_lossy(&entry.name).to_string();
        if name == "." || name == ".." {
            continue;
        }
        let path = format!("{at}/{name}");
        let (inode, raw) = fs
            .read_inode_raw(entry.ino)
            .unwrap_or_else(|e| panic!("the inode behind {path}: {e}"));
        if inode.is_dir() {
            out.push((path.clone(), format!("D {path}")));
            descend(fs, &inode, &raw, &path, out);
        } else if inode.is_symlink() {
            let target = fs.read_link(&inode, &raw).expect("a symlink target");
            out.push((
                path.clone(),
                format!("L {path} {}", String::from_utf8_lossy(&target)),
            ));
        } else {
            let body = fs.read_file(&inode, &raw).expect("a file's contents");
            out.push((
                path.clone(),
                format!("F {path} {} {:x}", body.len(), md5(&body)),
            ));
        }
    }
}

/// The digest the guest's `md5sum` prints, so the two can be compared
/// without shipping a hash implementation into the guest or a crate into
/// this one.
fn md5(bytes: &[u8]) -> Md5Hex {
    Md5Hex(md5_digest(bytes))
}

struct Md5Hex([u8; 16]);

impl std::fmt::LowerHex for Md5Hex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// RFC 1321, written out here rather than taken as a dependency: this is
/// a test comparing against `md5sum`, not a driver that needs a hash.
fn md5_digest(input: &[u8]) -> [u8; 16] {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();
    let mut msg = input.to_vec();
    let bitlen = (input.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bitlen.to_le_bytes());

    let (mut a0, mut b0, mut c0, mut d0) = (
        0x6745_2301u32,
        0xefcd_ab89u32,
        0x98ba_dcfeu32,
        0x1032_5476u32,
    );
    for chunk in msg.chunks_exact(64) {
        let m: Vec<u32> = chunk
            .chunks_exact(4)
            .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
            .collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    let mut out = [0u8; 16];
    for (i, word) in [a0, b0, c0, d0].iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
    out
}

#[test]
fn a_dirty_volume_mounts_and_reads_as_the_kernel_reads_it() {
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let pid = std::process::id();
    let crashed = scratch::Volume::empty(SUITE, &format!("dirty-{pid}.img"), 400 * 1024 * 1024);
    let copy = scratch::Volume::empty(SUITE, &format!("replayed-{pid}.img"), 400 * 1024 * 1024);
    let image = crashed.path().to_path_buf();
    let dirty = crashed.guest();
    let replayed = copy.guest();

    // THE CRASH IS `shutdown -f`: the log is flushed and the filesystem
    // is stopped before anything checkpoints it, which is the state a
    // power cut leaves and the one every structure below is read in.
    let Some(built) = kernel_run(&format!(
        r#"
        export LC_ALL=C
        mkfs.xfs -q -f {dirty} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {dirty} "$m" && echo MOUNT_OK
        mkdir "$m/settled"
        for i in $(seq 0 {settled_last}); do
            printf 'settled %04d\n' $i > "$m/settled/settled_$(printf %04d $i)"
        done
        sync
        # Everything from here on exists in the log and nowhere else.
        mkdir "$m/unflushed"
        for i in $(seq 0 {unflushed_last}); do
            : > "$m/unflushed/unflushed_$(printf %04d $i)"
        done
        rm "$m/settled/settled_0000"
        ln -s ../settled/settled_0001 "$m/unflushed/link"
        mkdir "$m/unflushed/deeper"
        : > "$m/unflushed/deeper/leaf"
        # BLOCKS FREED AND HANDED OUT AGAIN, inside the range about to be
        # replayed: a directory grown past its inode and then emptied
        # gives its blocks back, and what is made next takes them. Every
        # item logged for such a block before it changed hands describes
        # something it no longer is, and a replay that applied one would
        # write a directory block over an inode chunk.
        mkdir "$m/churn"
        for i in $(seq 0 {churn_last}); do
            mkdir "$m/churn/gone_$(printf %04d $i)"
        done
        rm -rf "$m/churn"
        # And what takes those blocks back is FILE DATA, which the log
        # does not carry: the blocks go to a file, the file is fsynced so
        # its bytes are on disk, and nothing later re-logs them. A replay
        # that applied the directory items from before the hand-over
        # would write a directory block over a file's contents, and the
        # file would read back as something else entirely.
        mkdir "$m/reused"
        for i in $(seq 0 {reused_last}); do
            xfs_io -f -c 'pwrite -S 0x5a 0 262144' -c fsync \
                "$m/reused/reused_$(printf %04d $i)" >/dev/null
        done
        # AN INODE UNLINKED WHILE IT IS STILL OPEN goes on the allocation
        # group's unlinked list, which the kernel maintains through the
        # inode *buffer* rather than the inode item beside it. It is in
        # no directory, so neither side lists it — what it is here for is
        # the buffer items it puts in the range.
        : > "$m/orphan"
        exec 9< "$m/orphan"
        rm "$m/orphan"
        xfs_io -x -c 'shutdown -f' "$m" && echo SHUTDOWN_OK
        exec 9<&-
        umount "$m" || umount -l "$m"
        rmdir "$m"
        cp {dirty} {replayed}
        m2=$(mktemp -d)
        if mount -o loop,nouuid {replayed} "$m2"; then
            echo REPLAY_MOUNT_OK
            cd "$m2"
            find . -mindepth 1 | sort | while read -r p; do
                if [ -L "$p" ]; then echo "ORACLE L $p $(readlink "$p")"
                elif [ -d "$p" ]; then echo "ORACLE D $p"
                else echo "ORACLE F $p $(stat -c %s "$p") $(md5sum < "$p" | cut -d' ' -f1)"
                fi
            done
            cd /
            umount "$m2"
        else
            echo REPLAY_MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m2"
        echo "REPAIR_BEGIN"
        xfs_repair -n {replayed} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#,
        settled_last = SETTLED - 1,
        unflushed_last = UNFLUSHED - 1,
        churn_last = CHURN - 1,
        reused_last = REUSED - 1,
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK") && built.contains("SHUTDOWN_OK"),
        "building the dirty volume failed:\n{built}"
    );
    assert!(
        built.contains("REPLAY_MOUNT_OK"),
        "the kernel would not replay the image, so there is nothing to compare against:\n{built}"
    );
    repair::assert_agreed(&built, "the copy the kernel replayed");
    let oracle: Vec<String> = built
        .lines()
        .filter_map(|l| l.strip_prefix("ORACLE "))
        .map(|l| l.trim().to_string())
        .collect();
    assert!(
        oracle.len() as u32 >= SETTLED + UNFLUSHED,
        "the kernel listed only {} names after replay, which is fewer than went in",
        oracle.len()
    );

    // THE IMAGE IS STILL DIRTY, and the driver is about to read it as it
    // is: the log's records were never applied to it.
    let before = md5_digest(&std::fs::read(&image).unwrap());
    let path = image.to_str().unwrap().to_string();
    let dev = Arc::new(FileDevice::open(&path).unwrap());
    let fs = Filesystem::mount(dev as Arc<dyn BlockRead>)
        .expect("a volume with a dirty log should mount, replaying into memory");
    let ours = walk(&fs);
    drop(fs);

    assert_eq!(
        md5_digest(&std::fs::read(&image).unwrap()),
        before,
        "a read-only mount wrote to the volume"
    );
    assert_eq!(
        ours.join("\n"),
        oracle.join("\n"),
        "this driver and the kernel disagree about what the replayed volume holds"
    );
}
