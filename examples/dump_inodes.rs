//! Dump an allocation group's inode chunks, so a before/after pair can
//! be compared by eye.
//!
//! `cargo run --example dump_inodes -- <image>`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agi;
use fs_xfs::inode_btree::{walk_from_agi, Which};
use fs_xfs::superblock::Superblock;

fn main() {
    let path = std::env::args().nth(1).expect("usage: dump_inodes <image>");
    let dev = FileDevice::open(&path).expect("open");
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).expect("read");
    let sb = Superblock::parse(&sbb).expect("superblock");

    let block = u64::from(sb.blocksize);
    for agno in 0..sb.agcount {
        let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;
        let mut raw = vec![0u8; usize::from(sb.sectsize)];
        dev.read_at(ag_start + 2 * u64::from(sb.sectsize), &mut raw)
            .expect("read agi");
        let agi = Agi::parse(&raw, &sb, agno).expect("agi");

        let read = |agblock: u32| {
            let mut buf = vec![0u8; sb.blocksize as usize];
            dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
            Ok(buf)
        };

        let all = walk_from_agi(&sb, &agi, Which::All, read)
            .expect("inobt")
            .unwrap_or_default();
        let free = walk_from_agi(&sb, &agi, Which::WithFreeInodes, read)
            .expect("finobt")
            .unwrap_or_default();

        if all.is_empty() {
            continue;
        }
        println!(
            "AG {agno}: count {} freecount {} newino {} root {} level {} free_root {} \
             free_level {}",
            agi.count,
            agi.freecount,
            agi.newino,
            agi.root,
            agi.level,
            agi.free_root,
            agi.free_level
        );
        for c in &all {
            println!(
                "  chunk {}: count {} freecount {} holemask {:#06x} free {:#018x}",
                c.startino, c.count, c.freecount, c.holemask, c.free
            );
        }
        println!(
            "  finobt holds {} chunk(s): {:?}",
            free.len(),
            free.iter()
                .map(|c| (c.startino, c.freecount))
                .collect::<Vec<_>>()
        );
    }
}
