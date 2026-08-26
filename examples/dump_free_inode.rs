//! Show what a free inode's on-disk core looks like, which decides
//! whether a create can read-modify-write it or has to build one.
//!
//! `cargo run --example dump_free_inode -- <image>`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agi;
use fs_xfs::inode_btree::{choose_free_inode, walk_from_agi, Which};
use fs_xfs::superblock::Superblock;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: dump_free_inode <image>");
    let dev = FileDevice::open(&path).expect("open");
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).expect("read");
    let sb = Superblock::parse(&sbb).expect("superblock");

    let block = u64::from(sb.blocksize);
    let ag_start = 0u64;
    let mut raw = vec![0u8; usize::from(sb.sectsize)];
    dev.read_at(ag_start + 2 * u64::from(sb.sectsize), &mut raw)
        .expect("agi");
    let agi = Agi::parse(&raw, &sb, 0).expect("agi");
    let read = |agblock: u32| {
        let mut buf = vec![0u8; sb.blocksize as usize];
        dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
        Ok(buf)
    };
    let chunks = walk_from_agi(&sb, &agi, Which::All, read)
        .expect("inobt")
        .unwrap_or_default();

    let Some((i, n)) = choose_free_inode(&chunks) else {
        println!("no free inode in AG 0");
        return;
    };
    let agino = chunks[i].startino + u32::from(n);
    // Rebuild the absolute inode number from the group and the
    // group-relative one.
    let ino = (0u64 << (sb.agblklog + sb.inopblog)) | u64::from(agino);
    println!("first free inode: agino {agino}, ino {ino}");

    let mut core = vec![0u8; usize::from(sb.inodesize)];
    let off = (u64::from(agino) >> sb.inopblog) * block
        + (u64::from(agino) & ((1u64 << sb.inopblog) - 1)) * u64::from(sb.inodesize);
    dev.read_at(off, &mut core).expect("read the inode");
    println!("  at offset {off}: first 64 bytes");
    for row in core[..64].chunks(16) {
        println!("    {row:02x?}");
    }
    let magic = u16::from_be_bytes(core[0..2].try_into().unwrap());
    println!(
        "  di_magic {magic:#06x} ({}), di_mode {:#o}, di_version {}, di_gen {}",
        if magic == 0x494e {
            "IN — initialised"
        } else {
            "not an inode"
        },
        u16::from_be_bytes(core[2..4].try_into().unwrap()),
        core[4],
        u32::from_be_bytes(core[92..96].try_into().unwrap())
    );
}
