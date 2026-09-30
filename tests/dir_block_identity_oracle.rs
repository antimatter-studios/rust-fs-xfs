//! A directory block the kernel refuses is refused here too (#287).
//!
//! Every v5 directory block carries a CRC32C, the address it was written
//! to, the filesystem UUID and the inode number of the directory that owns
//! it. The checksum catches damaged bits. The other three catch an intact
//! block reached through a stale or misdirected pointer. `dir::verify_data_block`
//! and `verify_da_block` check all four, and nothing called them. So the
//! driver listed a directory the kernel calls corrupt and resolved names in
//! it. Worse, a create rebuilt that block under the directory's own owner and
//! address, and recovery gave it a fresh checksum: the evidence was
//! overwritten by the edit.
//!
//! The kernel builds two directories: `/d` in block form, and `/l` with
//! enough entries for leaf form, so it has separate data blocks and a hash
//! index. One field of one block is then damaged at a time, and anything
//! other than the checksum is damaged with the checksum recomputed, so that
//! only the identity is wrong. The kernel is the judge that each damaged
//! block is corrupt: `ls` on the directory fails. The driver then has to
//! refuse to list it, refuse to look a name up in it, and refuse to create
//! in it.

mod common;

use common::{kernel_run, scratch, share};

/// Where this suite's scratch volume lives, under `.vm-share/scratch/`, out
/// of reach of the suites that scan the fixtures beside them (#223).
const SUITE: &str = "dir_block_identity_oracle";

use fs_core::{BlockRead, FileDevice};
use fs_xfs::Filesystem;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::Arc;

/// The file offset where a directory's hash index starts:
/// `XFS_DIR2_LEAF_OFFSET`, 32 GiB.
const LEAF_OFFSET: u64 = 1 << 35;

/// Offsets in `xfs_dir3_blk_hdr` (data and block-form blocks).
mod data_hdr {
    pub const CRC: usize = 4;
    pub const BLKNO: usize = 8;
    pub const UUID: usize = 24;
    pub const OWNER: usize = 40;
}

/// Offsets in `xfs_da3_blkinfo` (leaf and node blocks).
mod da_hdr {
    pub const CRC: usize = 12;
    pub const BLKNO: usize = 16;
}

/// One damage to one block: which directory, which of its blocks, what is
/// changed, and where the CRC lives so it can be recomputed afterwards.
struct Damage {
    what: &'static str,
    dir: &'static str,
    /// The directory's own file offset, in bytes, of the damaged block.
    file_offset: u64,
    field: usize,
    crc_at: usize,
    /// Leave the checksum wrong, rather than recomputing it.
    break_crc: bool,
    /// Whether listing the directory reads this block. A listing reads the
    /// data blocks and never the hash index, in the kernel and here, so a
    /// damaged index is refused by a lookup and not by a listing.
    listing_reads_it: bool,
}

const DAMAGES: [Damage; 6] = [
    Damage {
        what: "a block-form block's checksum",
        dir: "d",
        file_offset: 0,
        field: data_hdr::CRC,
        crc_at: data_hdr::CRC,
        break_crc: true,
        listing_reads_it: true,
    },
    Damage {
        what: "a block-form block's address",
        dir: "d",
        file_offset: 0,
        field: data_hdr::BLKNO + 7,
        crc_at: data_hdr::CRC,
        break_crc: false,
        listing_reads_it: true,
    },
    Damage {
        what: "a block-form block's UUID",
        dir: "d",
        file_offset: 0,
        field: data_hdr::UUID,
        crc_at: data_hdr::CRC,
        break_crc: false,
        listing_reads_it: true,
    },
    Damage {
        what: "a block-form block's owner",
        dir: "d",
        file_offset: 0,
        field: data_hdr::OWNER + 7,
        crc_at: data_hdr::CRC,
        break_crc: false,
        listing_reads_it: true,
    },
    Damage {
        what: "a leaf-form directory's first data block's owner",
        dir: "l",
        file_offset: 0,
        field: data_hdr::OWNER + 7,
        crc_at: data_hdr::CRC,
        break_crc: false,
        listing_reads_it: true,
    },
    // The hash index's ADDRESS, not its owner. The guest's kernel (6.1)
    // does not check a leaf block's owner -- that check arrived in 6.10 --
    // and resolved a name through a leaf with a foreign owner, so that
    // damage is not corruption to this oracle. Every kernel checks the
    // address in the leaf's verifier. The driver refuses both.
    Damage {
        what: "a leaf-form directory's hash index's address",
        dir: "l",
        file_offset: LEAF_OFFSET,
        field: da_hdr::BLKNO + 7,
        crc_at: da_hdr::CRC,
        break_crc: false,
        listing_reads_it: false,
    },
];

fn read_bytes(path: &Path, at: u64, len: usize) -> Vec<u8> {
    let mut f = std::fs::File::open(path).expect("open the volume");
    f.seek(SeekFrom::Start(at)).unwrap();
    let mut buf = vec![0u8; len];
    f.read_exact(&mut buf).unwrap();
    buf
}

fn write_bytes(path: &Path, at: u64, bytes: &[u8]) {
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the volume for writing");
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(bytes).unwrap();
    f.sync_all().unwrap();
}

#[test]
fn a_directory_block_the_kernel_refuses_is_refused_here() {
    // THE SHARED DIRECTORY IS ALWAYS THERE. `chore fixtures` makes it
    // before anything else runs, and this test writes its scratch volume
    // beside the fixtures. An absent share is that build not having
    // happened, which has to be seen rather than skipped.
    assert!(
        share().is_dir(),
        "{} is not there: the fixtures are gitignored and generated, and this test \
         writes its scratch volume beside them. `chore fixtures` builds the set and \
         makes the directory. Tests never skip on a missing fixture.",
        share().display()
    );
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        400 * 1024 * 1024,
    );
    let name = scratch.guest();
    let image = scratch.path().to_path_buf();

    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {name} "$m" && echo MOUNT_OK
        mkdir "$m/d" "$m/l"
        for i in $(seq 1 30); do : > "$m/d/entry_$i"; done
        for i in $(seq 1 400); do : > "$m/l/a_longer_entry_name_$i"; done
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );

    // WHERE EACH BLOCK IS, and what the undamaged volume answers.
    let blocks = {
        let fs = Filesystem::mount(
            Arc::new(FileDevice::open(&image).expect("open")) as Arc<dyn BlockRead>
        )
        .expect("mount the undamaged volume");
        let sb = fs.superblock().clone();
        let dir_block = u64::from(sb.dirblocksize());
        let mut blocks = Vec::new();
        for d in &DAMAGES {
            let dir = fs
                .lookup_path(&format!("/{}", d.dir))
                .expect("the directory");
            let (inode, raw) = fs.read_inode_raw(dir.ino).expect("its inode");
            let listed = fs
                .read_dir(&inode, &raw)
                .expect("the undamaged directory lists");
            assert!(!listed.is_empty(), "/{} is empty", d.dir);
            let extents = fs.data_extents(&inode, &raw).expect("its map");
            if d.dir == "d" {
                assert_eq!(
                    extents.len(),
                    1,
                    "/d is not in block form, so this case tests something else"
                );
            } else {
                assert!(
                    extents
                        .iter()
                        .any(|e| e.startoff * u64::from(sb.blocksize) >= LEAF_OFFSET),
                    "/l has no hash index block, so it is not in leaf form"
                );
            }
            let file_block = d.file_offset / u64::from(sb.blocksize);
            let e = extents
                .iter()
                .find(|e| e.startoff <= file_block && file_block < e.startoff + e.blockcount)
                .unwrap_or_else(|| panic!("/{} has nothing mapped at {}", d.dir, d.file_offset));
            let fsblock = e.startblock + (file_block - e.startoff);
            let first_name = listed[0].name.clone();
            blocks.push((
                dir.ino,
                sb.fsblock_offset(fsblock),
                dir_block as usize,
                first_name,
            ));
        }
        blocks
    };

    for (d, (ino, at, len, first_name)) in DAMAGES.iter().zip(&blocks) {
        let original = read_bytes(&image, *at, *len);
        let mut damaged = original.clone();
        damaged[d.field] ^= 0x5a;
        if !d.break_crc {
            damaged[d.crc_at..d.crc_at + 4].copy_from_slice(&[0; 4]);
            let crc = crc32c::crc32c(&damaged);
            damaged[d.crc_at..d.crc_at + 4].copy_from_slice(&crc.to_le_bytes());
        }
        write_bytes(&image, *at, &damaged);

        // THE KERNEL SAYS IT IS CORRUPT: a lookup through it fails, and a
        // listing fails exactly when the listing reads this block.
        let path = format!("/{}/{}", d.dir, String::from_utf8_lossy(first_name));
        let judged = kernel_run(&format!(
            r#"
            m=$(mktemp -d)
            mount -o loop,ro {name} "$m"
            if ls "$m/{dir}" > /dev/null 2>&1; then echo LS_OK; else echo LS_REFUSED; fi
            if stat "$m{path}" > /dev/null 2>&1; then echo STAT_OK; else echo STAT_REFUSED; fi
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
            rmdir "$m"
            echo DONE
            "#,
            dir = d.dir
        ));
        assert!(
            judged.contains("STAT_REFUSED"),
            "{}: the kernel still resolves {path}, so the damage is not corruption and \
             this case proves nothing:\n{judged}",
            d.what
        );
        assert_eq!(
            judged.contains("LS_REFUSED"),
            d.listing_reads_it,
            "{}: the kernel's listing of /{} is not what this case expects of it:\n{judged}",
            d.what,
            d.dir
        );

        // AND SO DOES THE DRIVER, on the same paths.
        let fs = Filesystem::mount(
            Arc::new(FileDevice::open(&image).expect("open")) as Arc<dyn BlockRead>
        )
        .expect("the volume itself still mounts");
        let (inode, raw) = fs.read_inode_raw(*ino).expect("the directory's inode");
        let listed = fs.read_dir(&inode, &raw);
        assert_eq!(
            listed.is_err(),
            d.listing_reads_it,
            "{}: listing /{} gave {:?}, where the kernel's listing {}",
            d.what,
            d.dir,
            listed.as_ref().map(|l| l.len()),
            if d.listing_reads_it {
                "is refused"
            } else {
                "succeeds"
            }
        );
        let found = fs.lookup_path(&path);
        assert!(
            found.is_err(),
            "{}: {path} resolved to {:?} through a block the kernel refuses",
            d.what,
            found.map(|i| i.ino)
        );
        drop(fs);

        let rw = Filesystem::mount_rw(Arc::new(FileDevice::open_rw(&image).expect("open rw")))
            .expect("a read-write mount of the volume");
        let created = rw.create_file(*ino, b"built_on_a_bad_block", 0o100644);
        assert!(
            created.is_err(),
            "{}: a create in /{} was journalled on a block the kernel refuses: {created:?}",
            d.what,
            d.dir
        );
        drop(rw);

        write_bytes(&image, *at, &original);
    }
}
