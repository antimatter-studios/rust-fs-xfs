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

/// A file whose attribute fork has blocks of its own: `names` small user
/// attributes, past what the inode holds, and one value of 3,000 bytes,
/// past what a leaf keeps beside its name, so stored in blocks of its own.
fn file_with_attribute_blocks(fs: &Filesystem, name: &[u8], names: usize) -> u64 {
    use fs_xfs::attr_write::XattrMode;
    let root = fs.superblock().rootino;
    let ino = fs.create_file(root, name, 0o600).unwrap().0;
    for i in 0..names {
        fs.set_xattr(
            ino,
            format!("user.name_{i:03}").as_bytes(),
            format!("value_{i:03}").as_bytes(),
            XattrMode::Create,
        )
        .unwrap();
    }
    fs.set_xattr(ino, b"user.remote", &[0x5a; 3000], XattrMode::Create)
        .unwrap();
    let inode = fs.read_inode(ino).unwrap();
    assert!(
        inode.aformat != fs_xfs::inode::Format::Local && inode.anextents > 0 && inode.nblocks > 0,
        "the attribute fork this case is about has blocks of its own: {:?} with {} extents \
         and {} blocks",
        inode.aformat,
        inode.anextents,
        inode.nblocks
    );
    ino
}

/// The inode `ino` was freed with everything it held: no mode, no links,
/// no blocks and no attribute fork, and its generation moved on.
fn assert_freed(fs: &Filesystem, ino: u64, gen_before: u32) {
    let freed = fs.read_inode(ino).unwrap();
    assert_eq!(
        (freed.mode, freed.nlink, freed.nblocks, freed.forkoff),
        (0, 0, 0, 0)
    );
    assert_eq!(freed.gen, gen_before.wrapping_add(1));
}

/// The volume on `device`, mounted afresh so the log is replayed, has no
/// block that is both free and owned, and none owned by nobody.
fn assert_clean_after_replay(device: &Arc<SparseDevice>, what: &str) {
    let fresh = Filesystem::mount(device.clone()).expect("remount");
    let report = fs_xfs::check::check(&fresh);
    assert!(report.is_clean(), "{what}: {:#?}", report.findings);
}

/// The last name of a file whose attribute fork has blocks of its own --
/// a leaf or a node, and a remote value -- frees them with the inode
/// (#495): each goes back to free space, and the volume replayed from the
/// log is consistent. It was refused before anything was written.
#[test]
fn final_unlink_gives_back_the_attribute_blocks() {
    for (blocksize, names) in [(1024, 60), (4096, 40)] {
        let (fs, device) = filesystem(blocksize);
        let root = fs.superblock().rootino;
        let ino = file_with_attribute_blocks(&fs, b"victim", names);
        let before = fs.read_inode(ino).unwrap();
        let free_blocks_before = free_blocks(&fs);
        fs.unlink_file(root, b"victim").unwrap_or_else(|e| {
            panic!("{blocksize}: the last name of a file with attribute blocks: {e:?}")
        });
        assert_eq!(fs.lookup_path("/victim"), Err(fs_xfs::Error::NotFound));
        assert_eq!(
            free_blocks(&fs),
            free_blocks_before + before.nblocks,
            "{blocksize}: every attribute block goes back to free space"
        );
        assert_freed(&fs, ino, before.gen);
        drop(fs);
        assert_clean_after_replay(&device, &format!("{blocksize}: after the unlink"));
    }
}

/// The same for a rename that replaces the last name of such a file
/// (#495).
#[test]
fn a_rename_over_a_last_name_gives_back_the_attribute_blocks() {
    for (blocksize, names) in [(1024, 60), (4096, 40)] {
        let (fs, device) = filesystem(blocksize);
        let root = fs.superblock().rootino;
        let ino = file_with_attribute_blocks(&fs, b"victim", names);
        fs.create_file(root, b"replacer", 0o600).unwrap();
        let before = fs.read_inode(ino).unwrap();
        let free_blocks_before = free_blocks(&fs);
        fs.rename(root, b"replacer", root, b"victim")
            .unwrap_or_else(|e| {
                panic!(
                    "{blocksize}: replacing the last name of a file with attribute blocks: {e:?}"
                )
            });
        assert_ne!(fs.lookup_path("/victim").unwrap().ino, ino);
        assert_eq!(
            free_blocks(&fs),
            free_blocks_before + before.nblocks,
            "{blocksize}: every attribute block goes back to free space"
        );
        assert_freed(&fs, ino, before.gen);
        drop(fs);
        assert_clean_after_replay(&device, &format!("{blocksize}: after the rename"));
    }
}
