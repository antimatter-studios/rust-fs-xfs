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
//! The same changes are made on inodes whose attribute fork is past short
//! form (#488), which the kernel builds with `setfattr`: a leaf of 30 user
//! attributes and a node of 300. Each change rebuilds the fork's blocks in
//! the same record as the directory entry; one rename rebuilds both, and
//! one link moves a fork from short form to a leaf with its new pointer.
//! The kernel has to replay those blocks, list every pointer, and still
//! report every user attribute.
//!
//! And the last name of an inode whose attribute fork has blocks of its
//! own frees all of them (#495): a leaf and remote values mapped by a
//! B+tree, each unlinked, and a node, replaced by a rename. The guest's
//! `xfs_repair -n` checks that every one of those blocks is free again
//! after the replay, and owned by nothing.
//!
//! One inode's fork also holds a value stored in blocks of its own (#501),
//! which is not logged: the driver writes it before the record that maps
//! it, into blocks that were free before that record began. One of its
//! changes is a rename that replaces a file with data, whose blocks the
//! same record gives back. After the replay `getfattr` has to read the
//! value back unchanged.

mod common;

use common::{guest_quote, kernel_run, repair, scratch, PARENT_XFSPROGS_BIN};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::attr::ParentPointer;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::Arc;

const SUITE: &str = "parent_pointer_replay_kernel";

/// Two names, each 175 bytes: one's parent pointer fits short form beside
/// nothing else, and two do not, so the second link moves the fork to a
/// leaf (#488).
fn long_names() -> [String; 2] {
    ["a", "b"].map(|c| format!("long-{c}-").repeat(25))
}

/// Every name that must exist after the replay, and the pointers its
/// inode must carry: (entry name, path of the directory holding it).
fn survivors() -> Vec<(String, Vec<(String, String)>)> {
    let [a, b] = long_names();
    let both = vec![
        (a.clone(), "/made".to_string()),
        (b.clone(), "/made".to_string()),
    ];
    let fixed: &[(&str, &[(&str, &str)])] = &[
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
        // #488: the leaf-form inode, named twice in /made since its moved
        // name replaced one of the node-form inode's, which keeps one.
        (
            "/made/leaf-link",
            &[("leaf-link", "/made"), ("node-link", "/made")],
        ),
        (
            "/made/node-link",
            &[("leaf-link", "/made"), ("node-link", "/made")],
        ),
        ("/made/node-moved", &[("node-moved", "/made")]),
        // #495: the file a rename put in place of a freed one.
        ("/node-last", &[("node-last", "/")]),
        // #501: the inode with a remote value, named twice since its second
        // name replaced a file with data.
        (
            "/remote.txt",
            &[("remote.txt", "/"), ("remote-victim", "/made")],
        ),
        (
            "/made/remote-victim",
            &[("remote.txt", "/"), ("remote-victim", "/made")],
        ),
    ];
    let mut all: Vec<(String, Vec<(String, String)>)> = fixed
        .iter()
        .map(|(path, links)| {
            (
                path.to_string(),
                links
                    .iter()
                    .map(|(name, dir)| (name.to_string(), dir.to_string()))
                    .collect(),
            )
        })
        .collect();
    all.push((format!("/made/{a}"), both.clone()));
    all.push((format!("/made/{b}"), both));
    all
}

/// Names the changes removed, which must not come back with the replay.
const GONE: &[&str] = &[
    "/top.txt",
    "/dir",
    "/made/top-link",
    "/made/replacer",
    "/leaf.txt",
    "/leaf-too",
    "/made/leaf-moved",
    "/node.txt",
    "/node-too",
    "/leaf-last",
    "/tree-last",
    "/made/over-node",
    "/remote-too",
    "/made/remote-link",
];

/// The inodes whose forks were rebuilt past short form, and how many user
/// attributes each must still have after the replay (#488).
const KEPT_ATTRIBUTES: &[(&str, usize)] = &[
    ("/made/leaf-link", 30),
    ("/made/node-moved", 300),
    ("/remote.txt", 1),
];

/// The value of `/remote.txt`'s one user attribute, `user.big`: 3000 bytes,
/// past what a leaf keeps beside its name, so stored in blocks of its own
/// (#501). It is that many `r`s.
const REMOTE_VALUE_LEN: usize = 3000;

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
        # Past short form (#488): 30 user attributes are a leaf, 300 more
        # than one leaf holds, so a node. Each file has a second name.
        echo leaf > "$m/leaf.txt"
        for i in $(seq -w 0 29); do setfattr -n "user.name_$i" -v "value_$i" "$m/leaf.txt"; done
        ln "$m/leaf.txt" "$m/leaf-too"
        echo node > "$m/node.txt"
        for i in $(seq -w 0 299); do setfattr -n "user.name_$i" -v "value_$i" "$m/node.txt"; done
        ln "$m/node.txt" "$m/node-too"
        # One name each, so removing it frees the inode with its attribute
        # blocks (#495). Sixty remote values with every other one removed
        # leave thirty extents with holes between them, more than the
        # inode holds, so their map is a B+tree.
        echo leaf > "$m/leaf-last"
        for i in $(seq -w 0 29); do setfattr -n "user.name_$i" -v "value_$i" "$m/leaf-last"; done
        echo node > "$m/node-last"
        for i in $(seq -w 0 299); do setfattr -n "user.name_$i" -v "value_$i" "$m/node-last"; done
        echo tree > "$m/tree-last"
        big=$(head -c 3000 /dev/zero | tr '\0' v)
        for i in $(seq -w 0 59); do setfattr -n "user.big_$i" -v "$big" "$m/tree-last"; done
        for i in $(seq -w 1 2 59); do setfattr -x "user.big_$i" "$m/tree-last"; done
        # A remote value (#501): 3000 bytes do not fit a leaf beside a name.
        echo remote > "$m/remote.txt"
        setfattr -n user.big -v "$(head -c {REMOTE_VALUE_LEN} /dev/zero | tr '\0' r)" "$m/remote.txt"
        ln "$m/remote.txt" "$m/remote-too"
        umount "$m" || {{ echo UMOUNT_FAILED; exit 1; }}
        rmdir "$m"
        # The form each fork is in, as xfs_db reads attribute block 0.
        for f in leaf.txt node.txt leaf-last node-last remote.txt; do
            out=$({PARENT_XFSPROGS_BIN}/xfs_db -r -c "path /$f" -c "ablock 0" -c print {image} 2>&1 || true)
            if echo "$out" | grep -q '^hdr.level = '; then form=node
            elif echo "$out" | grep -q '^hdr.count = '; then form=leaf
            else form=neither; fi
            echo "FORM /$f $form"
        done
        out=$({PARENT_XFSPROGS_BIN}/xfs_db -r -c "path /tree-last" -c "print core.aformat" {image} 2>&1 || true)
        echo "AFORMAT /tree-last $out"
        echo DONE
        "#
    ));
    assert!(!built.contains("UMOUNT_FAILED"), "{built}");
    assert!(
        built.contains("DONE"),
        "the guest did not build the volume:\n{built}"
    );
    for (file, form) in [
        ("/leaf.txt", "leaf"),
        ("/node.txt", "node"),
        ("/leaf-last", "leaf"),
        ("/node-last", "node"),
        ("/remote.txt", "leaf"),
    ] {
        assert!(
            built.contains(&format!("FORM {file} {form}")),
            "{file}: the kernel did not build a {form}-form attribute fork:\n{built}"
        );
    }
    assert!(
        built.contains("AFORMAT /tree-last core.aformat = 3 (btree)"),
        "/tree-last: the kernel did not map its attribute fork with a B+tree:\n{built}"
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
        // PAST SHORT FORM (#488): a leaf, then a node, each linked, moved
        // to another directory, and unlinked from one of its names.
        fs.link(ino("/leaf.txt"), made, b"leaf-link")
            .expect("link an inode whose attribute fork is a leaf");
        fs.rename(root, b"leaf.txt", made, b"leaf-moved")
            .expect("move an inode whose attribute fork is a leaf");
        fs.unlink_file(root, b"leaf-too")
            .expect("unlink a name of an inode whose attribute fork is a leaf");
        fs.link(ino("/node.txt"), made, b"node-link")
            .expect("link an inode whose attribute fork is a node");
        fs.rename(root, b"node.txt", made, b"node-moved")
            .expect("move an inode whose attribute fork is a node");
        fs.unlink_file(root, b"node-too")
            .expect("unlink a name of an inode whose attribute fork is a node");
        // Both forks rebuilt in one record: the leaf-form inode's name
        // replaces one of the node-form inode's two.
        fs.rename(made, b"leaf-moved", made, b"node-link")
            .expect("a rename whose moved and replaced inodes both have forks past short form");
        // A fork that crosses from short form to a leaf with the pointer
        // its second name adds.
        let [a, b] = long_names();
        let (long, _) = fs
            .create_file(made, a.as_bytes(), 0o100644)
            .expect("create");
        fs.link(long, made, b.as_bytes())
            .expect("a link whose parent pointer moves the attribute fork past short form");
        // THE LAST NAME (#495): the inode freed with every block of its
        // attribute fork, by an unlink and by a rename that replaces it.
        fs.unlink_file(root, b"leaf-last")
            .expect("unlink the last name of an inode whose attribute fork is a leaf");
        fs.unlink_file(root, b"tree-last").expect(
            "unlink the last name of an inode whose attribute fork maps remote values with a \
             B+tree",
        );
        fs.create_file(made, b"over-node", 0o100644)
            .expect("create");
        fs.rename(made, b"over-node", root, b"node-last")
            .expect("replace the last name of an inode whose attribute fork is a node");
        // A REMOTE VALUE (#501): linked, then a name of it replaces a file
        // whose data the same record gives back, then unlinked from one.
        fs.link(ino("/remote.txt"), made, b"remote-link")
            .expect("link an inode with a remote attribute value");
        let (victim, _) = fs
            .create_file(made, b"remote-victim", 0o100644)
            .expect("create");
        fs.write(victim, 0, &[0x33; 64 * 1024]).expect("write");
        fs.rename(root, b"remote-too", made, b"remote-victim")
            .expect("a name of an inode with a remote attribute value replaces a file with data");
        fs.unlink_file(made, b"remote-link")
            .expect("unlink a name of an inode with a remote attribute value");
        // DROPPED, NOT UNMOUNTED. An unmount checkpoints and leaves a
        // clean log (#439); without one, the records are all there is.
    }

    let survivors = survivors();
    let paths: String = survivors
        .iter()
        .map(|(path, _)| format!(" .{path}"))
        .collect();
    let kept: String = KEPT_ATTRIBUTES
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
            # The user attributes beside the rebuilt parent pointers.
            for p in{kept}; do
                echo "XATTRS ${{p#.}} $(getfattr -d "$p" 2>/dev/null | grep -c '^user\.' || true)"
            done
            # The remote value (#501): its length, and how many of its
            # bytes are not the `r` it was made of.
            for p in ./remote.txt ./made/remote-victim; do
                v=$(getfattr --only-values -n user.big "$p" 2>/dev/null | od -An -v -tx1 | tr -s ' \n' ' ')
                echo "BIG ${{p#.}} $(echo $v | wc -w) $(echo $v | tr ' ' '\n' | grep -cv '^72$' || true)"
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
    for (path, links) in &survivors {
        if path == "/made/sym" {
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
            .map(|(name, dir)| (name.clone(), kino(dir)))
            .collect();
        want.sort();
        assert_eq!(
            got, want,
            "{path}: the kernel's parent pointers after the replay disagree with its \
             directory entries:\n{out}"
        );
    }

    for path in ["/remote.txt", "/made/remote-victim"] {
        assert!(
            out.lines()
                .any(|l| l == format!("BIG {path} {REMOTE_VALUE_LEN} 0")),
            "{path}: getfattr does not read its {REMOTE_VALUE_LEN}-byte remote value back \
             unchanged after the replay:\n{out}"
        );
    }
    for (path, count) in KEPT_ATTRIBUTES {
        assert!(
            out.lines().any(|l| l == format!("XATTRS {path} {count}")),
            "{path}: the kernel does not list its {count} user attributes after the \
             replay:\n{out}"
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
    for (path, links) in &survivors {
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
        let mut want: Vec<ParentPointer> = links
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
        want.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(
            got, want,
            "{path}: the driver's reading of its parent pointers after the replay"
        );
    }
}
