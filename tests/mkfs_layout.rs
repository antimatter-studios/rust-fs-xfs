//! `mkfs.xfs` lays a filesystem out exactly as the standard formatter does
//! (#338).
//!
//! The fixtures `xfs-default`, `xfs-2k` and `xfs-1k` were made by the
//! standard `mkfs.xfs` with nothing but a block size chosen. Our
//! `mkfs.xfs`, given a device of the same size and the fixture's UUID, has
//! to write every allocation group's header block — the superblock copy,
//! AGF, AGI and AGFL — and its five btree roots byte for byte as they are
//! in the fixture.
//!
//! The UUID is the only thing a format chooses that those structures
//! record, so with it fixed there is nothing left that may differ. The
//! inodes are not compared: they carry the time of the format. The kernel
//! and `xfs_repair` grade those in `cli_mkfs_kernel`.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, scratch};

const SUITE: &str = "mkfs_layout";

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

fn read(path: &std::path::Path, offset: u64, len: usize) -> Vec<u8> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).unwrap();
    f.seek(SeekFrom::Start(offset)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

fn uuid_arg(sb: &[u8]) -> String {
    sb[32..48].iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn every_group_header_and_btree_root_is_the_standard_formatters() {
    for name in ["xfs-default.img", "xfs-2k.img", "xfs-1k.img"] {
        let reference = fixture(name);
        assert!(
            reference.is_file(),
            "the fixture {} is not there. `chore fixtures` builds the set; a run \
             without it fails rather than skips.",
            reference.display()
        );
        let sb = read(&reference, 0, 512);
        let blocksize = be32(&sb, 4);
        let agblocks = be32(&sb, 84);
        let agcount = be32(&sb, 88);
        let size = std::fs::metadata(&reference).unwrap().len();

        let ours = scratch::Volume::empty(SUITE, &format!("{}-{name}", std::process::id()), size);
        let size_arg = format!("size={blocksize}");
        let uuid = format!("uuid={}", uuid_arg(&sb));
        ok(tool("mkfs.xfs")
            .args(["-q", "-b", &size_arg, "-m", &uuid])
            .arg(ours.path()));

        // The header block(s) — four sectors, so two blocks at 1 KiB — and
        // the five roots after them.
        let header_blocks = (4 * 512u64).div_ceil(u64::from(blocksize));
        let span = ((header_blocks + 5) * u64::from(blocksize)) as usize;
        for ag in 0..agcount {
            let at = u64::from(ag) * u64::from(agblocks) * u64::from(blocksize);
            let want = read(&reference, at, span);
            let got = read(ours.path(), at, span);
            let differ: Vec<usize> = (0..span).filter(|&i| want[i] != got[i]).collect();
            assert!(
                differ.is_empty(),
                "{name}: allocation group {ag} differs from the standard formatter's at \
                 {} byte(s), the first at offset {} (block {}, byte {})",
                differ.len(),
                differ[0],
                differ[0] / blocksize as usize,
                differ[0] % blocksize as usize
            );
        }
    }
}
