//! A write this driver logs must produce a file the Linux kernel can
//! read back.
//!
//! This is the first write that allocates, and the first whose proof is
//! the file's own contents rather than its metadata. Everything a
//! truncate has to get right it has to get right in reverse — but with
//! one thing a truncate never has to do at all: the bytes have to be
//! *somewhere*, and the extent record has to point at them.
//!
//! That is what makes the check strong. An extent record naming the
//! wrong block, an allocation taken from a run that was not free, a
//! filesystem-block number packed with the group in the wrong place —
//! none of those stop the record checksumming, and none of them stop the
//! kernel replaying it. What they do is give back a file full of
//! somebody else's data, or zeroes, and comparing the contents is what
//! notices.
//!
//! # The shape of the proof
//!
//! - the file reads back **byte for byte** — only a correct extent
//!   record pointing at correctly written blocks does that;
//! - the blocks it now holds have left the group's free space, so a
//!   later allocation cannot be given them too;
//! - the neighbouring files still read back whole, which is what catches
//!   blocks handed out twice;
//! - and `xfs_repair` afterwards is what catches trees that are
//!   plausible on their own and inconsistent with each other.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Build them with
//! `./scripts/vm-build-truncate-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

/// The file the fixtures leave empty, which this one fills.
const VICTIM: &str = "/victim";

fn share() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share")
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn vm_run(script: &str) -> Option<String> {
    let out = Command::new(repo().join("scripts/vm.sh"))
        .arg("run")
        .arg(script)
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() {
        eprintln!(
            "vm.sh run failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
        return None;
    }
    assert!(
        stdout.contains("DONE"),
        "the VM script did not run to completion:\n{stdout}"
    );
    Some(stdout)
}

/// A working image in the shared folder, removed when it goes out of
/// scope. Every other suite treats each `.img` there as a fixture, so
/// one left behind fails unrelated tests.
struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let path = share().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Scratch(path)
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

/// Contents that cannot be mistaken for anything already on the
/// filesystem.
///
/// The fixture's other files are all zeroes, so a write that landed
/// nowhere, or an extent pointing at a neighbour's blocks, would read
/// back as zeroes — and a test comparing against zeroes would pass. This
/// is deliberately not zeroes, and deliberately varies along its length
/// so that a partial or misaligned write shows up as a position rather
/// than merely as a mismatch.
fn payload(bytes: usize) -> Vec<u8> {
    (0..bytes).map(|i| (i % 251 + 1) as u8).collect()
}

/// Write into the empty victim of a truncated fixture, then have the
/// kernel read it back.
fn write_and_replay(case: &str, bytes: usize) -> Option<()> {
    // The after-image is the one whose victim was truncated to nothing.
    let source = share().join(format!("xfstrunc-{case}-after.img"));
    if !source.exists() {
        return None;
    }
    let name = format!("xfs-write-{case}-scratch.img");
    let scratch = Scratch::from(&source, &name);
    let img = scratch.path();

    let data = payload(bytes);

    let victim = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
        let found = fs.lookup_path(VICTIM).expect("the victim is present");
        assert_eq!(
            found.size, 0,
            "{case}: the fixture's victim should be empty before this writes to it"
        );
        found.ino
    };

    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let lsn = fs
            .write_into_empty_file(victim, &data)
            .unwrap_or_else(|e| panic!("{case}: the write must be accepted: {e}"));
        assert_ne!(lsn, 0, "a record must be given a sequence number");
    }

    // The data blocks are on disk, but the inode still says the file is
    // empty — nothing but the log claims them yet.
    {
        let dev = FileDevice::open(img).expect("open read-only");
        let err = Filesystem::mount(Arc::new(dev))
            .err()
            .expect("a log with an unreplayed record must not mount");
        assert!(
            matches!(err, fs_xfs::Error::DirtyLog),
            "{case}: expected the log to read as dirty, got {err}"
        );
    }

    let script = format!(
        r#"
        cp /share/{name} /tmp/w.img
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop /tmp/w.img "$m"; then
            echo "SIZE $(stat -c %s "$m/victim")"
            echo "SUM $(md5sum < "$m/victim" | cut -d' ' -f1)"
            ok=yes
            for f in f1 f2 f4 f5; do
                [ -e "$m/$f" ] || continue
                cksum "$m/$f" >/dev/null 2>&1 || ok=no
                [ "$(stat -c %s "$m/$f")" = "1048576" ] || ok=no
            done
            echo "NEIGHBOURS $ok"
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        echo "REPAIR_BEGIN"
        xfs_repair -n /tmp/w.img 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        rm -f /tmp/w.img
        echo "DONE"
        "#
    );

    let out = vm_run(&script)?;

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused the filesystem after the write was logged:\n{out}"
    );

    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("{case}: the VM did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };

    assert_eq!(
        field("SIZE"),
        bytes.to_string(),
        "{case}: the file should be {bytes} bytes after the replay\n{out}"
    );

    // The contents themselves. Compared as a digest so a mismatch does
    // not print a megabyte, and computed here from the same bytes that
    // were handed to the driver.
    let expected = format!("{:032x}", md5(&data));
    assert_eq!(
        field("SUM"),
        expected,
        "{case}: the file read back different bytes than were written — the extent \
         record points somewhere other than where the data went\n{out}"
    );

    assert_eq!(
        field("NEIGHBOURS"),
        "yes",
        "{case}: a neighbouring file no longer reads back correctly, which is what \
         blocks handed out twice looks like\n{out}"
    );

    let repair: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        repair.contains("REPAIR_RC=0"),
        "{case}: xfs_repair found something wrong after the replay:\n{repair}"
    );

    Some(())
}

/// MD5, so the test can state the digest it expects rather than trusting
/// the VM to compute both sides.
///
/// Written out rather than pulled in: a dependency for sixty lines used
/// by one assertion is a dependency to keep updated forever, and this is
/// not being used for anything a weakness in MD5 could affect — the two
/// inputs being compared are both ours.
fn md5(data: &[u8]) -> u128 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    let k: Vec<u32> = (0..64)
        .map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32)
        .collect();

    let mut msg = data.to_vec();
    let bitlen = (data.len() as u64).wrapping_mul(8);
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
    out[0..4].copy_from_slice(&a0.to_le_bytes());
    out[4..8].copy_from_slice(&b0.to_le_bytes());
    out[8..12].copy_from_slice(&c0.to_le_bytes());
    out[12..16].copy_from_slice(&d0.to_le_bytes());
    u128::from_be_bytes(out)
}

/// A file written by this driver, read back by the kernel.
#[test]
fn the_kernel_reads_back_a_file_this_driver_wrote() {
    let mut ran = Vec::new();
    // Sizes that land either side of a block boundary, so the tail
    // padding is exercised as well as the whole-block case.
    for (case, bytes) in [
        ("lone", 4096),
        ("after", 100),
        ("before", 4097),
        ("between", 40_000),
    ] {
        match write_and_replay(case, bytes) {
            Some(()) => ran.push((case, bytes)),
            None => eprintln!("{case}: fixture or VM unavailable — skipped"),
        }
    }
    if ran.is_empty() {
        eprintln!(
            "no truncate fixtures or no VM; build them with \
             ./scripts/vm-build-truncate-fixtures.sh"
        );
        return;
    }
    eprintln!("the kernel read back what this driver wrote for: {ran:?}");
}

#[cfg(test)]
mod tests {
    use super::md5;

    /// The digest against the published test vectors, so a fault in it
    /// cannot quietly make the comparison above vacuous.
    #[test]
    fn the_digest_matches_the_published_vectors() {
        for (input, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            (
                "The quick brown fox jumps over the lazy dog",
                "9e107d9d372bb6826bd81d3542a419d6",
            ),
        ] {
            assert_eq!(
                format!("{:032x}", md5(input.as_bytes())),
                expected,
                "{input:?}"
            );
        }
    }
}
