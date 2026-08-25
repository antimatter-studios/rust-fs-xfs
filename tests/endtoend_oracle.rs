//! End-to-end: read filesystems the Linux kernel wrote, and require the
//! contents back byte-identical.
//!
//! Every other test in this crate checks one structure against one
//! reference reading of it. This one checks the whole driver against the
//! only judgement that ultimately matters — whether a filesystem the
//! kernel produced comes back out the way the kernel sees it.
//!
//! The fixtures are built by mounting a real XFS filesystem, writing a
//! tree into it, and unmounting. The manifest beside each image is
//! generated **inside Linux by the kernel's own driver**: one line per
//! path, with type, size and SHA-256. Nothing in this repository decides
//! what the right answer is.
//!
//! Coverage in the tree: a small file, a multi-block file, an 8 MiB file
//! spanning many extents, a sparse file that is mostly holes, a short
//! symlink stored inline in its inode and a long one stored in a block,
//! nested directories, and 400 entries in one directory — enough to push
//! that directory out of short form into a block or leaf layout.
//!
//! Fixtures are gitignored, so this skips on a fresh clone. Generate
//! them with `./scripts/vm-build-data-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::inode::FileType;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// What the kernel says one path is.
#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    /// Size and SHA-256 of the contents.
    File(u64, String),
    /// Link target.
    Link(String),
}

fn parse_manifest(text: &str) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() != 4 {
            continue;
        }
        let entry = match parts[1] {
            "dir" => Entry::Dir,
            "link" => Entry::Link(parts[3].to_string()),
            "file" => Entry::File(parts[2].parse().unwrap_or(0), parts[3].to_string()),
            _ => continue,
        };
        out.insert(parts[0].to_string(), entry);
    }
    out
}

fn sha256_hex(bytes: &[u8]) -> String {
    // A local implementation keeps this test free of a dependency that
    // would exist only for it.
    use std::fmt::Write;
    let digest = sha256(bytes);
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Minimal SHA-256. Only used to compare against the kernel's own
/// `sha256sum` output, so it is checked by the comparison itself: a
/// wrong implementation would fail every file rather than pass silently.
fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes(chunk[i * 4..i * 4 + 4].try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        let updated: [u32; 8] = [a, b, c, d, e, f, g, hh];
        for (slot, v) in h.iter_mut().zip(updated) {
            *slot = slot.wrapping_add(v);
        }
    }
    let mut out = [0u8; 32];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

fn fixtures() -> Vec<(String, PathBuf, PathBuf)> {
    let share = Path::new(env!("CARGO_MANIFEST_DIR")).join(".vm-share");
    let Ok(entries) = std::fs::read_dir(&share) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = p.file_stem().map(|s| s.to_string_lossy().into_owned());
        let Some(name) = name else { continue };
        if !name.starts_with("xfsdata-") || p.extension().and_then(|s| s.to_str()) != Some("img") {
            continue;
        }
        let manifest = p.with_extension("manifest");
        if manifest.exists() {
            out.push((name, p, manifest));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Walk the whole tree with this driver and compare against the
/// kernel-generated manifest, path by path.
#[test]
fn reads_back_what_the_kernel_wrote() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no xfsdata-* fixtures in .vm-share — skipping");
        return;
    }

    for (label, img, manifest_path) in &fixtures {
        let expected = parse_manifest(&std::fs::read_to_string(manifest_path).unwrap());
        assert!(!expected.is_empty(), "{label}: manifest is empty");

        let dev = FileDevice::open(img)
            .unwrap_or_else(|e| panic!("{label}: opening the image failed: {e}"));
        let fs = Filesystem::mount(Arc::new(dev))
            .unwrap_or_else(|e| panic!("{label}: mount failed: {e}"));

        let mut seen = BTreeMap::new();
        walk(&fs, "", &mut seen, label);

        // Every path the kernel reported must be present and identical.
        let mut checked = 0usize;
        for (path, want) in &expected {
            let got = seen.get(path).unwrap_or_else(|| {
                panic!("{label}: this driver did not find `{path}`, which the kernel listed")
            });
            assert_eq!(
                got, want,
                "{label}: `{path}` differs from what the kernel reported"
            );
            checked += 1;
        }

        // And nothing invented: no path this driver produced may be
        // absent from the kernel's listing.
        for path in seen.keys() {
            assert!(
                expected.contains_key(path),
                "{label}: this driver reported `{path}`, which the kernel's listing does not contain"
            );
        }

        eprintln!("  {label}: {checked} paths match the kernel exactly");
    }
}

/// Recursively list `dir` through the driver, recording every entry.
fn walk(fs: &Filesystem, dir: &str, out: &mut BTreeMap<String, Entry>, label: &str) {
    let path = if dir.is_empty() { "/" } else { dir };
    let entries = fs
        .list_path(path)
        .unwrap_or_else(|e| panic!("{label}: listing `{path}` failed: {e}"));

    for e in entries {
        let name = String::from_utf8_lossy(&e.name).into_owned();
        if name == "." || name == ".." {
            continue;
        }
        let child = format!("{dir}/{name}");

        let (inode, raw) = fs
            .read_inode_raw(e.ino)
            .unwrap_or_else(|err| panic!("{label}: reading inode for `{child}` failed: {err}"));

        match inode.file_type() {
            Some(FileType::Directory) => {
                out.insert(child.clone(), Entry::Dir);
                walk(fs, &child, out, label);
            }
            Some(FileType::Symlink) => {
                let target = fs
                    .read_link(&inode, &raw)
                    .unwrap_or_else(|err| panic!("{label}: readlink `{child}` failed: {err}"));
                out.insert(
                    child,
                    Entry::Link(String::from_utf8_lossy(&target).into_owned()),
                );
            }
            Some(FileType::Regular) => {
                let data = fs
                    .read_file(&inode, &raw)
                    .unwrap_or_else(|err| panic!("{label}: reading `{child}` failed: {err}"));
                assert_eq!(
                    data.len() as u64,
                    inode.size,
                    "{label}: `{child}` read {} bytes but the inode says {}",
                    data.len(),
                    inode.size
                );
                out.insert(child, Entry::File(inode.size, sha256_hex(&data)));
            }
            other => panic!("{label}: `{child}` has unexpected type {other:?}"),
        }
    }
}

/// A sparse file is mostly holes. Its content must read back as zeros
/// rather than as whatever previously occupied those blocks — the
/// manifest's hash covers this, but assert it directly too, because a
/// driver that leaked stale data here would be a security bug rather
/// than a correctness one.
#[test]
fn sparse_regions_read_as_zeros() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }
    for (label, img, _) in &fixtures {
        let dev = FileDevice::open(img).expect("open");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        let Ok(inode) = fs.lookup_path("/sparse.bin") else {
            continue;
        };
        let (inode, raw) = fs.read_inode_raw(inode.ino).expect("inode");
        let data = fs.read_file(&inode, &raw).expect("read");
        assert_eq!(data.len(), 10 * 1024 * 1024, "{label}: sparse file size");
        assert!(
            data.iter().all(|&b| b == 0),
            "{label}: a hole in the sparse file did not read back as zeros"
        );
        eprintln!("  {label}: 10 MiB sparse file reads as zeros");
    }
}

/// Reading at an offset must agree with reading the whole file and
/// slicing it. This catches an offset mishandled in the extent walk,
/// which a whole-file read would never expose.
#[test]
fn partial_reads_agree_with_whole_file_reads() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no fixtures — skipping");
        return;
    }
    for (label, img, _) in &fixtures {
        let dev = FileDevice::open(img).expect("open");
        let fs = Filesystem::mount(Arc::new(dev)).expect("mount");
        let Ok(found) = fs.lookup_path("/large.bin") else {
            continue;
        };
        let (inode, raw) = fs.read_inode_raw(found.ino).expect("inode");
        let whole = fs.read_file(&inode, &raw).expect("read whole");

        // Offsets chosen to straddle block and extent boundaries rather
        // than to sit neatly on them.
        for &(off, len) in &[
            (0u64, 100usize),
            (4095, 2),
            (4096, 4096),
            (1_000_003, 9973),
            (whole.len() as u64 - 10, 10),
        ] {
            let mut buf = vec![0u8; len];
            let n = fs.read_at(&inode, &raw, off, &mut buf).expect("read_at");
            assert_eq!(
                &buf[..n],
                &whole[off as usize..off as usize + n],
                "{label}: read_at({off}, {len}) disagrees with the whole-file read"
            );
        }
        eprintln!("  {label}: partial reads agree with the whole-file read");
    }
}

/// The fixture must actually contain a B+tree-format data fork.
///
/// `reads_back_what_the_kernel_wrote` covers the bmbt walker only
/// because one file in the tree is fragmented enough to have outgrown an
/// inline extent list. Nothing about that is guaranteed: a different
/// mkfs default, a larger inode, or a kernel that packs extents more
/// tightly could keep the same file inline, and the end-to-end test
/// would still pass — while quietly no longer exercising the walker at
/// all. So the fixture builder records each candidate's fork format
/// straight out of the reference debugger, and this requires at least
/// one of them to be a B+tree.
#[test]
fn the_fixture_still_contains_a_btree_fork() {
    let fixtures = fixtures();
    if fixtures.is_empty() {
        eprintln!("no xfsdata-* fixtures in .vm-share — skipping");
        return;
    }
    for (label, img, _) in &fixtures {
        let forks = img.with_extension("bmbt");
        let Ok(text) = std::fs::read_to_string(&forks) else {
            panic!(
                "{label}: no {} — regenerate the fixtures with \
                 ./scripts/vm-build-data-fixtures.sh",
                forks.display()
            );
        };
        let btree: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("btree"))
            .map(|l| l.split('\t').next().unwrap_or(l))
            .collect();
        assert!(
            !btree.is_empty(),
            "{label}: no file in this fixture has a B+tree data fork, so nothing here \
             exercises the bmbt walker. The debugger reported:\n{text}"
        );
        eprintln!("{label}: B+tree data forks: {}", btree.join(", "));
    }
}
