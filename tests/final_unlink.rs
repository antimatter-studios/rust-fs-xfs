//! The final unlink of a written file frees its inode and every block it
//! held, on 1 KiB and 4 KiB blocks (#384). Absorbed from the earlier #384
//! branch; the kernel's verdict on the same is `hardlinks_oracle.rs`.
use fs_core::{BlockDevice, BlockRead};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct SparseDevice(Mutex<BTreeMap<u64, Vec<u8>>>);

impl BlockRead for SparseDevice {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let pages = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let within = at as usize % 4096;
            let count = (4096 - within).min(buf.len() - done);
            if let Some(page) = pages.get(&(at / 4096)) {
                buf[done..done + count].copy_from_slice(&page[within..within + count]);
            } else {
                buf[done..done + count].fill(0);
            }
            done += count;
        }
        Ok(())
    }

    fn size_bytes(&self) -> u64 {
        320 * 1024 * 1024
    }
}

impl BlockDevice for SparseDevice {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        let mut pages = self.0.lock().unwrap();
        let mut done = 0;
        while done < buf.len() {
            let at = offset + done as u64;
            let within = at as usize % 4096;
            let count = (4096 - within).min(buf.len() - done);
            let bytes = &buf[done..done + count];
            if bytes.iter().any(|byte| *byte != 0) || pages.contains_key(&(at / 4096)) {
                pages.entry(at / 4096).or_insert_with(|| vec![0; 4096])[within..within + count]
                    .copy_from_slice(bytes);
            }
            done += count;
        }
        Ok(())
    }

    fn is_writable(&self) -> bool {
        true
    }
}

fn filesystem(blocksize: u32) -> (Filesystem, Arc<SparseDevice>) {
    let device = Arc::new(SparseDevice::default());
    fs_xfs::mkfs::format(
        device.as_ref(),
        &fs_xfs::mkfs::Options {
            block_size: blocksize,
            ..Default::default()
        },
    )
    .unwrap();
    let fs = Filesystem::mount_rw(device.clone()).unwrap();
    (fs, device)
}

fn free_blocks(fs: &Filesystem) -> u64 {
    (0..fs.superblock().agcount)
        .map(|agno| u64::from(fs.read_agf(agno).unwrap().freeblks))
        .sum()
}

#[test]
fn final_unlink_frees_a_populated_inode_and_its_blocks() {
    for blocksize in [1024, 4096] {
        let (fs, _) = filesystem(blocksize);
        let root = fs.superblock().rootino;
        let ino = fs.create_file(root, b"victim", 0o600).unwrap().0;
        let payload = vec![0x5a; blocksize as usize + 17];
        fs.write_into_empty_file(ino, &payload).unwrap();
        let before = fs.read_inode(ino).unwrap();
        let agno = fs.superblock().split_ino(ino).0;
        let free_inodes = fs.read_agi(agno).unwrap().freecount;
        let free_blocks_before = free_blocks(&fs);
        fs.unlink_file(root, b"victim")
            .expect("the final unlink must free file data with the inode");
        assert_eq!(fs.lookup_path("/victim"), Err(fs_xfs::Error::NotFound));
        assert_eq!(fs.read_agi(agno).unwrap().freecount, free_inodes + 1);
        assert_eq!(free_blocks(&fs), free_blocks_before + before.nblocks);
        let freed = fs.read_inode(ino).unwrap();
        assert_eq!((freed.mode, freed.nlink, freed.nblocks), (0, 0, 0));
        assert_eq!(freed.gen, before.gen.wrapping_add(1));
    }
}
