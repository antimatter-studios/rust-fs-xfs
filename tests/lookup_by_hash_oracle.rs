//! A lookup goes through the directory's hash index rather than listing
//! the directory (#95).
//!
//! `mkfs.xfs -p` builds one directory of each shape past short form: block
//! form (a few names), leaf form (hundreds) and node form (20,000). Each
//! holds a pair of names with the same hash, `aaaaaaaa` and ``aaaqaaa` ``: the
//! first four bytes of each are rotated 28 bits before the last four are
//! mixed in, so flipping bit 4 of byte 3 and bit 0 of byte 7 cancels.
//!
//! In every shape, every listed name must look up to its listed inode, the
//! colliding pair to their own contents, and absent names to nothing. In the
//! node-form directory one lookup must also reach the device only a handful
//! of times, where listing it reads every data block. Skips when xfsprogs is
//! not installed.

use fs_core::{CountingDevice, FileDevice};
use fs_xfs::Filesystem;
use std::fmt::Write as _;
use std::process::Command;
use std::sync::Arc;

const COLLIDING: [&str; 2] = ["aaaaaaaa", "aaaqaaa`"];

fn mkfs_available() -> bool {
    Command::new("mkfs.xfs")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[test]
fn a_lookup_goes_through_the_hash_index() {
    if !mkfs_available() {
        eprintln!("skip: xfsprogs not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("fs_xfs_lookup_hash_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let empty = root.join("empty");
    std::fs::write(&empty, b"").unwrap();
    for name in COLLIDING {
        std::fs::write(
            root.join(format!(
                "content_{}",
                name.len() + name.as_bytes()[3] as usize
            )),
            name,
        )
        .unwrap();
    }
    let content_of = |name: &str| {
        root.join(format!(
            "content_{}",
            name.len() + name.as_bytes()[3] as usize
        ))
    };

    let shapes = [("block", 10usize), ("leaf", 400), ("node", 20_000)];
    let mut proto = String::from("/dev/null\n0 0\nd--755 0 0\n");
    for (dir, count) in shapes {
        writeln!(proto, "{dir} d--755 0 0").unwrap();
        for i in 0..count {
            writeln!(proto, "name_{i:05} ---644 0 0 {}", empty.display()).unwrap();
        }
        for name in COLLIDING {
            writeln!(proto, "{name} ---644 0 0 {}", content_of(name).display()).unwrap();
        }
        proto.push_str("$\n");
    }
    proto.push_str("$\n");
    let proto_path = root.join("proto");
    std::fs::write(&proto_path, proto).unwrap();

    let image = root.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(512 * 1024 * 1024))
        .unwrap();
    let out = Command::new("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(&proto_path)
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let counting = Arc::new(CountingDevice::new(Arc::new(
        FileDevice::open(image.to_str().unwrap()).unwrap(),
    )));
    let fs = Filesystem::mount(counting.clone()).expect("mount");

    for (dir, count) in shapes {
        let d = fs.open(&format!("/{dir}")).expect(dir);
        let listed = fs.read_dir(d.inode(), d.raw()).expect("read_dir");
        assert_eq!(listed.len(), count + COLLIDING.len(), "[{dir}] listing");
        for entry in &listed {
            let inode = fs
                .lookup(d.inode(), d.raw(), &entry.name)
                .unwrap_or_else(|e| {
                    panic!("[{dir}] {}: {e:?}", String::from_utf8_lossy(&entry.name))
                });
            assert_eq!(
                inode.ino,
                entry.ino,
                "[{dir}] {}",
                String::from_utf8_lossy(&entry.name)
            );
        }
        for name in COLLIDING {
            let file = fs.open(&format!("/{dir}/{name}")).expect(name);
            assert_eq!(
                fs.read_file(file.inode(), file.raw()).unwrap(),
                name.as_bytes(),
                "[{dir}] {name} resolved to its twin"
            );
        }
        for absent in ["name_99999", "aaaaaaab", "", ".", ".."] {
            assert!(
                fs.lookup(d.inode(), d.raw(), absent.as_bytes()).is_err(),
                "[{dir}] {absent:?} is not there"
            );
        }
    }

    // The cost, in calls to the device, of one lookup in 20,000 names, on
    // a mount with no cache so every block read is a call.
    drop(fs);
    let counting = Arc::new(CountingDevice::new(Arc::new(
        FileDevice::open(image.to_str().unwrap()).unwrap(),
    )));
    let fs = Filesystem::mount_with_cache(counting.clone(), 0).expect("mount uncached");
    let node = fs.open("/node").unwrap();
    let before = counting.reads();
    fs.lookup(node.inode(), node.raw(), b"name_19999").unwrap();
    let lookup_reads = counting.reads() - before;
    let before = counting.reads();
    fs.read_dir(node.inode(), node.raw()).unwrap();
    let listing_reads = counting.reads() - before;
    eprintln!("node-form lookup: {lookup_reads} reads; listing: {listing_reads}");
    assert!(
        lookup_reads <= 8,
        "one lookup made {lookup_reads} device reads (listing makes {listing_reads}): \
         it is not going through the index"
    );

    let _ = std::fs::remove_dir_all(&root);
}
