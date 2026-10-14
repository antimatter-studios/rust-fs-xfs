//! Parent pointers the driver journals are ones the Linux kernel replays,
//! and after the replay every name and its parent pointer agree (#373).
//!
//! RUN ON THE DEBIAN 13 GUEST, `chore test:kernel-new`. Debian 12's 6.1
//! kernel refuses to mount a volume with the parent-pointer bit, so it
//! can neither replay this driver's log nor read the result. Run on that
//! guest this fails at the mount, saying so, rather than passing.
//!
//! `tests/parent_exchrange_oracle.rs` grades the same kinds of change
//! after a clean unmount, which checkpoints them: there, nothing replays.
//! Here the driver makes them and is dropped WITHOUT unmounting, so they
//! exist only as committed records in the log. Then:
//!
//! - the kernel has to say it ran recovery, which is what proves the
//!   changes reached the volume through the log and nothing else;
//! - the kernel's own view of the namespace, the names and the inode
//!   each resolves to, is the forward side;
//! - the kernel's `XFS_IOC_GETPARENTS`, through `xfs_io -c parent`, is
//!   the reverse side, and each name has to have exactly the pointers
//!   its links imply;
//! - the guest's `xfs_repair -n` (xfsprogs 6.13) checks every parent
//!   pointer against the directory entries, and must find nothing to
//!   change;
//! - and the driver, reading the replayed volume back, has to agree with
//!   the kernel about both sides.
//!
//! A link to an inode whose attributes are past short form is refused
//! before anything is written; `tests/parent_exchrange_oracle.rs` holds
//! that. Nothing here reaches it, because every inode the kernel makes
//! below has only its own parent pointers.

mod common;

use common::{guest_quote, kernel_run, repair, scratch};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::attr::ParentPointer;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::Arc;

const SUITE: &str = "parent_pointer_replay_kernel";

/// Every name that must exist after the replay, and the pointers its
/// inode must carry: (entry name, path of the directory holding it).
const SURVIVORS: &[(&str, &[(&str, &str)])] = &[
    ("/made", &[("made", "/")]),
    ("/made/new.txt", &[("new.txt", "/made")]),
    ("/made/sym", &[("sym", "/made")]),
    ("/made/top-moved.txt", &[("top-moved.txt", "/made")]),
    ("/made/dir-moved", &[("dir-moved", "/made")]),
    (
        "/made/dir-moved/hello.txt",
        &[("hello.txt", "/made/dir-moved")],
    ),
    ("/victim", &[("victim", "/")]),
    ("/victim-too", &[("victim-too", "/")]),
];

/// Names the changes removed, which must not come back with the replay.
const GONE: &[&str] = &["/top.txt", "/dir", "/made/top-link", "/made/replacer"];

#[test]
fn parent_pointers_the_driver_journals_are_replayed_by_the_kernel() {
    let volume = scratch::Volume::empty(
        SUITE,
        &format!("{}-parent.img", std::process::id()),
        300 * 1024 * 1024,
    );
    let image = guest_quote(&volume.guest());

    // The kernel makes the starting namespace, so every inode the driver
    // changes starts with the parent pointers the kernel gave it.
    let built = kernel_run(&format!(
        r#"
        set -e
        mkfs.xfs -q -f -n parent=1 {image}
        m=$(mktemp -d)
        mount -o loop {image} "$m"
        echo top > "$m/top.txt"
        mkdir "$m/dir"
        echo hello > "$m/dir/hello.txt"
        echo victim > "$m/victim"
        ln "$m/victim" "$m/victim-too"
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(!built.contains("UMOUNT_FAILED"), "{built}");
    assert!(
        built.contains("DONE"),
        "the guest did not build the volume:\n{built}"
    );

    {
        let dev = FileDevice::open_rw(volume.path()).expect("open");
        let fs = Filesystem::mount_rw(Arc::new(dev) as Arc<dyn BlockDevice>)
            .expect("a parent-pointer volume mounts read-write");
        let root = fs.superblock().rootino;
        let ino = |path: &str| {
            fs.lookup_path(path)
                .unwrap_or_else(|e| panic!("{path}: {e:?}"))
                .ino
        };
        let (made, _) = fs.create_directory(root, b"made", 0o040755).expect("mkdir");
        fs.create_file(made, b"new.txt", 0o100644).expect("create");
        fs.create_symlink(made, b"sym", b"new.txt")
            .expect("symlink");
        fs.link(ino("/top.txt"), made, b"top-link").expect("link");
        fs.rename(root, b"top.txt", made, b"top-moved.txt")
            .expect("move a file with two links");
        fs.unlink_file(made, b"top-link")
            .expect("unlink one of two links");
        fs.rename(root, b"dir", made, b"dir-moved")
            .expect("move a directory");
        fs.create_file(made, b"replacer", 0o100644).expect("create");
        fs.rename(made, b"replacer", root, b"victim")
            .expect("replace a name another link keeps alive");
        // DROPPED, NOT UNMOUNTED. An unmount checkpoints and leaves a
        // clean log (#439); without one, the records are all there is.
    }

    let paths: String = SURVIVORS
        .iter()
        .map(|(path, _)| format!(" .{path}"))
        .collect();
    let gone: String = GONE.iter().map(|path| format!(" .{path}")).collect();
    let out = kernel_run(&format!(
        r#"
        img=$(mktemp -u /tmp/parent-XXXXXX.img)
        cp --sparse=always {image} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            if dmesg | grep -q "Starting recovery"; then
                echo RECOVERED
            else
                echo NO_RECOVERY
                dmesg | tail -8
            fi
            cd "$m"
            echo "INO / $(stat -c %i .)"
            for p in{paths}; do
                if [ -e "$p" ] || [ -L "$p" ]; then
                    echo "INO ${{p#.}} $(stat -c %i "$p")"
                    # A symlink would be followed by the open; the driver
                    # reads its pointers below instead.
                    if [ ! -L "$p" ]; then
                        xfs_io -r -c parent "$p" 2>&1 | sed "s|^|PPTR ${{p#.}} |"
                    fi
                else
                    echo "MISSING ${{p#.}}"
                fi
            done
            for p in{gone}; do
                if [ -e "$p" ] || [ -L "$p" ]; then echo "PRESENT ${{p#.}}"; fi
            done
            cd /
            umount "$m" || echo UMOUNT_FAILED
        else
            echo MOUNT_FAILED
            dmesg | tail -8
        fi
        rmdir "$m"
        {repair}
        # The replayed volume, back where the driver can read it.
        cp --sparse=always "$img" {image}
        rm -f "$img"
        echo DONE
        "#,
        repair = repair::script("\"$img\""),
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the volume, which on a 6.1 kernel is the parent-pointer bit \
         itself: run this on the Debian 13 guest (chore test:kernel-new):\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    assert!(
        out.contains("DONE"),
        "the guest script did not finish:\n{out}"
    );
    assert!(
        out.contains("RECOVERED"),
        "the kernel ran no recovery, so the changes reached the disk some other way than \
         the log and nothing here was replayed:\n{out}"
    );
    if let Some(line) = out.lines().find(|l| l.starts_with("MISSING ")) {
        panic!("{line} after the replay:\n{out}");
    }
    if let Some(line) = out.lines().find(|l| l.starts_with("PRESENT ")) {
        panic!("{line}: a removed name came back with the replay:\n{out}");
    }

    // The forward side, as the kernel resolved it.
    let kernel_ino: BTreeMap<&str, u64> = out
        .lines()
        .filter_map(|l| l.strip_prefix("INO "))
        .filter_map(|l| {
            let (path, ino) = l.rsplit_once(' ')?;
            Some((path, ino.trim().parse().ok()?))
        })
        .collect();
    let kino = |path: &str| {
        *kernel_ino
            .get(path)
            .unwrap_or_else(|| panic!("the kernel gave no inode for {path}:\n{out}"))
    };
    assert_ne!(
        kino("/victim"),
        kino("/victim-too"),
        "the rename did not replace /victim:\n{out}"
    );

    // The reverse side, as the kernel's GETPARENTS reports it.
    for (path, links) in SURVIVORS {
        if *path == "/made/sym" {
            continue;
        }
        let prefix = format!("PPTR {path} ");
        let mut got: Vec<(String, u64)> = Vec::new();
        let mut parent = None;
        for line in out.lines().filter_map(|l| l.strip_prefix(&prefix)) {
            let Some((field, value)) = line.split_once('=') else {
                continue;
            };
            match field.trim() {
                "p_ino" => parent = value.trim().parse().ok(),
                // p_namelen precedes p_name in xfs_io's output. Match
                // the field exactly so it cannot consume the inode.
                "p_name" => {
                    let (start, end) = (value.find('"'), value.rfind('"'));
                    if let (Some(start), Some(end), Some(p)) = (start, end, parent.take()) {
                        if end > start {
                            got.push((value[start + 1..end].to_string(), p));
                        }
                    }
                }
                _ => {}
            }
        }
        got.sort();
        let mut want: Vec<(String, u64)> = links
            .iter()
            .map(|(name, dir)| (name.to_string(), kino(dir)))
            .collect();
        want.sort();
        assert_eq!(
            got, want,
            "{path}: the kernel's parent pointers after the replay disagree with its \
             directory entries:\n{out}"
        );
    }

    repair::assert_agreed(
        &out,
        "the volume after the kernel replayed the driver's namespace changes",
    );
    let said = repair::report(&out);
    assert!(
        !said.to_ascii_lowercase().contains("would"),
        "xfs_repair would change the replayed volume:\n{said}"
    );

    // The driver reading the replayed volume agrees with the kernel.
    let fs = Filesystem::mount(Arc::new(FileDevice::open(volume.path()).unwrap()))
        .expect("the replayed volume mounts");
    for (path, links) in SURVIVORS {
        let file = fs
            .open(path)
            .unwrap_or_else(|e| panic!("{path} after the replay: {e:?}"));
        assert_eq!(
            file.inode().ino,
            kino(path),
            "{path}: the driver and the kernel resolve it to different inodes"
        );
        let mut got = fs.parent_pointers(file.inode(), file.raw()).unwrap();
        got.sort_by(|a, b| a.name.cmp(&b.name));
        let want: Vec<ParentPointer> = links
            .iter()
            .map(|(name, dir)| {
                let dir = fs.open(dir).unwrap();
                ParentPointer {
                    name: name.as_bytes().to_vec(),
                    parent: dir.inode().ino,
                    generation: dir.inode().gen,
                }
            })
            .collect();
        assert_eq!(
            got, want,
            "{path}: the driver's reading of its parent pointers after the replay"
        );
    }
}
