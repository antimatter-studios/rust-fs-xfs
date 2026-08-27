//! Decode a block-form directory block, field by field.
//!
//! Written to read what the kernel produced when it converted a
//! short-form directory, because that block is what a conversion has to
//! learn to write and a hex dump of 4 KiB does not show its shape.
//!
//! `cargo run --example dump_dirblock -- <image> <dir-path>`

use fs_core::{BlockRead, FileDevice};
use fs_xfs::format::dir::{
    offsets, XFS_DIR2_BLOCK_TAIL_SIZE, XFS_DIR2_DATA_ALIGN, XFS_DIR2_DATA_FD_COUNT,
    XFS_DIR2_DATA_FREE_SIZE, XFS_DIR2_LEAF_ENTRY_SIZE, XFS_DIR3_DATA_HDR_SIZE,
};
use fs_xfs::Filesystem;
use std::sync::Arc;

fn be16(b: &[u8], i: usize) -> u16 {
    u16::from_be_bytes(b[i..i + 2].try_into().unwrap())
}
fn be32(b: &[u8], i: usize) -> u32 {
    u32::from_be_bytes(b[i..i + 4].try_into().unwrap())
}
fn be64(b: &[u8], i: usize) -> u64 {
    u64::from_be_bytes(b[i..i + 8].try_into().unwrap())
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let [img, path] = <[String; 2]>::try_from(a)
        .unwrap_or_else(|_| panic!("usage: dump_dirblock <image> <dir-path>"));

    let dev = Arc::new(FileDevice::open(&img).expect("open"));
    let fs = Filesystem::mount(dev.clone()).expect("mount");
    let found = fs.lookup_path(&path).expect("find the directory");
    let (inode, raw) = fs.read_inode_raw(found.ino).expect("read the inode");
    let sb = fs.superblock();

    println!(
        "{path}: ino {} format {:?} size {} nblocks {}",
        inode.ino, inode.format, inode.size, inode.nblocks
    );
    let extents = fs.data_extents(&inode, &raw).expect("extents");
    let e = extents.first().expect("the directory has a block");
    println!("  extent: fsblock {} count {}", e.startblock, e.blockcount);

    let dirblocksize = (u64::from(sb.blocksize) << sb.dirblklog) as usize;
    let mut block = vec![0u8; dirblocksize];
    dev.read_at(sb.fsblock_offset(e.startblock), &mut block)
        .expect("read the directory block");

    // ---- header -----------------------------------------------------
    use offsets::dir3_blk as h;
    println!("\nheader ({XFS_DIR3_DATA_HDR_SIZE} bytes)");
    let magic = be32(&block, h::MAGIC);
    println!(
        "  magic    {:#010x}  {}",
        magic,
        String::from_utf8_lossy(&magic.to_be_bytes())
    );
    println!("  blkno    {}  (basic blocks)", be64(&block, h::BLKNO));
    println!("  lsn      {:#018x}", be64(&block, h::LSN));
    println!("  owner    {}", be64(&block, h::OWNER));
    println!(
        "  crc      {:#010x}",
        u32::from_le_bytes(block[h::CRC..h::CRC + 4].try_into().unwrap())
    );

    // ---- best-free array --------------------------------------------
    let bf = offsets::data_hdr::V5_BESTFREE;
    println!("\nbestfree (the three largest free regions, largest first)");
    for i in 0..XFS_DIR2_DATA_FD_COUNT {
        let at = bf + i * XFS_DIR2_DATA_FREE_SIZE;
        let (off, len) = (be16(&block, at), be16(&block, at + 2));
        println!(
            "  [{i}] offset {off:5}  length {len:5}{}",
            if off == 0 && len == 0 {
                "   (unused)"
            } else {
                ""
            }
        );
    }

    // ---- entries and free records -----------------------------------
    let tail_at = dirblocksize - XFS_DIR2_BLOCK_TAIL_SIZE;
    let count = be32(&block, tail_at + offsets::block_tail::COUNT) as usize;
    let stale = be32(&block, tail_at + offsets::block_tail::STALE) as usize;
    let index_start = tail_at - count * XFS_DIR2_LEAF_ENTRY_SIZE;

    println!("\nentries (data space {XFS_DIR3_DATA_HDR_SIZE}..{index_start})");
    let mut at = XFS_DIR3_DATA_HDR_SIZE;
    while at < index_start {
        // A free record is marked by 0xffff where an entry's inode
        // number would start; nothing else can appear there.
        if be16(&block, at) == 0xffff {
            let len = be16(&block, at + 2) as usize;
            let tag = be16(&block, at + len - 2);
            println!(
                "  @{at:5} FREE   length {len:5}  tag {tag:5}{}",
                if tag as usize == at {
                    ""
                } else {
                    "   <-- TAG MISMATCH"
                }
            );
            at += len;
            continue;
        }
        let ino = be64(&block, at);
        let namelen = block[at + offsets::data_entry::NAMELEN] as usize;
        let name = &block[at + offsets::data_entry::NAME..at + offsets::data_entry::NAME + namelen];
        // ftype sits after the name; the tag after that, and the whole
        // record is rounded up to 8.
        let ftype = block[at + offsets::data_entry::NAME + namelen];
        let unrounded = 8 + 1 + namelen + 1 + 2;
        let len = unrounded.div_ceil(XFS_DIR2_DATA_ALIGN) * XFS_DIR2_DATA_ALIGN;
        let tag = be16(&block, at + len - 2);
        println!(
            "  @{at:5} entry  ino {ino:6} ftype {ftype} len {len:3} tag {tag:5}{}  {:?}",
            if tag as usize == at {
                ""
            } else {
                "  <-- TAG MISMATCH"
            },
            String::from_utf8_lossy(name)
        );
        at += len;
    }

    // ---- hash index --------------------------------------------------
    println!("\nhash index ({count} records, {stale} stale, at {index_start})");
    for i in 0..count {
        let at = index_start + i * XFS_DIR2_LEAF_ENTRY_SIZE;
        let hash = be32(&block, at + offsets::leaf_entry::HASHVAL);
        let addr = be32(&block, at + offsets::leaf_entry::ADDRESS);
        println!(
            "  [{i:3}] hash {hash:#010x}  address {addr:5}  (byte {})",
            addr as usize * 8
        );
    }
    println!("\ntail @{tail_at}: count {count}, stale {stale}");
}
