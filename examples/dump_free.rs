//! Dump an allocation group's free space, so a before/after pair can be
//! compared by eye.
//!
//! `cargo run --example dump_free -- <image> [victim-path]`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::ag::Agf;
use fs_xfs::alloc_btree::{walk_from_agf, Order};
use fs_xfs::superblock::Superblock;
use fs_xfs::Filesystem;
use std::sync::Arc;

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: dump_free <image> [victim]");
    let victim = args.next();

    let dev = Arc::new(FileDevice::open(&path).expect("open"));
    let mut sbb = vec![0u8; 4096];
    dev.read_at(0, &mut sbb).expect("read");
    let sb = Superblock::parse(&sbb).expect("superblock");

    if let Some(victim) = victim {
        let fs = Filesystem::mount(dev.clone()).expect("mount");
        match fs.lookup_path(&victim) {
            Ok(inode) => {
                let (inode, raw) = fs.read_inode_raw(inode.ino).expect("inode");
                println!(
                    "{victim}: ino {} size {} nblocks {} nextents {} format {:?}",
                    inode.ino, inode.size, inode.nblocks, inode.nextents, inode.format
                );
                if let Ok(extents) = fs.data_extents(&inode, &raw) {
                    for e in &extents {
                        println!(
                            "  extent: fsblock {} count {} offset {}",
                            e.startblock, e.blockcount, e.startoff
                        );
                    }
                }
            }
            Err(e) => println!("{victim}: {e}"),
        }
    }

    let block = u64::from(sb.blocksize);
    for agno in 0..sb.agcount {
        let ag_start = u64::from(agno) * u64::from(sb.agblocks) * block;
        let mut raw = vec![0u8; usize::from(sb.sectsize)];
        dev.read_at(ag_start + u64::from(sb.sectsize), &mut raw)
            .expect("read agf");
        let agf = Agf::parse(&raw, &sb, agno).expect("agf");

        let read = |agblock: u32| {
            let mut buf = vec![0u8; sb.blocksize as usize];
            dev.read_at(ag_start + u64::from(agblock) * block, &mut buf)?;
            Ok(buf)
        };
        let by_block = walk_from_agf(&sb, &agf, Order::ByBlock, read).expect("bnobt");

        println!(
            "AG {agno}: freeblks {} longest {} btreeblks {} flcount {} roots {:?} levels {:?}",
            agf.freeblks, agf.longest, agf.btreeblks, agf.flcount, agf.roots, agf.levels
        );
        for e in &by_block {
            println!("  free: {}+{}", e.startblock, e.blockcount);
        }
    }
}
