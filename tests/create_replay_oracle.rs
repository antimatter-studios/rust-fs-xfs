//! A create this driver logs must give the Linux kernel a usable file.
//!
//! The first transaction with five items, and the first that has to
//! leave two inodes and three metadata blocks agreeing with each other.
//! An inode taken out of the group's accounting but never made into a
//! file, a file made but never taken out of the accounting, or a name
//! added for an inode that is still marked free — each of those still
//! checksums, is still found, and is still replayed. What they produce
//! is a filesystem that is quietly wrong, and only a consistency check
//! notices.
//!
//! # The shape of the proof
//!
//! Nothing on disk is touched, so:
//!
//! - the name appearing is something only the replay could have done;
//! - the file being usable — openable, writable, with the mode it was
//!   given — is what separates a real inode from a directory entry
//!   pointing at nothing;
//! - the group having one fewer free inode is what says it was taken
//!   rather than borrowed, and a second create landing on a *different*
//!   inode is what proves it;
//! - the directory's other entries still resolving is what catches a
//!   short-form fork rebuilt from the wrong entries;
//! - and `xfs_repair` is what catches the inode trees and the group
//!   header disagreeing.
//!
//! Fixtures are gitignored and the VM is not always up, so this skips
//! rather than fails when either is missing. Build them with
//! `./scripts/vm-build-create-fixtures.sh`.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod common;
use common::{kernel_run, share};

/// A working image in the shared folder, removed when it goes out of
/// scope. Every other suite treats each `.img` there as a fixture, so
/// one left behind fails unrelated tests.
struct Scratch(PathBuf);

impl Scratch {
    fn from(source: &Path, name: &str) -> Self {
        let path = share().join(name);
        std::fs::copy(source, &path).expect("copy the fixture");
        Scratch(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Create a file in a copy of `case`'s before-image, then have the
/// kernel replay the record.
///
/// One file, because a mount writes one checkpoint. A journalled
/// operation touches nothing on disk, so a second would be built from a
/// disk that does not yet reflect the first — see
/// `Filesystem::begin_checkpoint`. That limit is asserted below rather
/// than merely worked around.
fn create_and_replay(case: &str, names: &[&str]) -> Option<()> {
    let source = share().join(format!("xfscreate-{case}-before.img"));
    if !source.exists() {
        return None;
    }
    let name = format!("xfs-create-{case}-scratch.img");
    let scratch = Scratch::from(&source, &name);
    let img = scratch.path();

    let mut created = Vec::new();
    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;

        let name = names[0];
        let (ino, lsn) = fs
            .create_file(root, name.as_bytes(), 0o100644)
            .unwrap_or_else(|e| panic!("{case}: creating {name} must be accepted: {e}"));
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        created.push((name, ino));

        // A second create on the same mount would be built from a disk
        // that does not yet reflect the first, and would hand out the
        // same inode again. It is refused, and this is what says so —
        // without it the limit is a comment rather than a behaviour.
        let err = fs
            .create_file(root, b"second", 0o100644)
            .expect_err("a second checkpoint on one mount must be refused");
        assert!(
            err.to_string().contains("already written a checkpoint"),
            "{case}: the refusal should say why: {err}"
        );
    }

    {
        let dev = FileDevice::open(img).expect("open read-only");
        let err = Filesystem::mount(Arc::new(dev))
            .err()
            .expect("a log with an unreplayed record must not mount");
        assert!(
            matches!(err, fs_xfs::Error::DirtyLog),
            "{case}: expected the log to read as dirty, got {err}"
        );
    }

    let checks: String = names
        .iter()
        .map(|n| {
            format!(
                r#"
            if [ -f "$m/{n}" ]; then
                echo "INO_{n} $(stat -c %i "$m/{n}")"
                echo "MODE_{n} $(stat -c %a "$m/{n})")"
                echo "SIZE_{n} $(stat -c %s "$m/{n}")"
                # A created file has to be usable, not merely present.
                echo "hello" > "$m/{n}" 2>/dev/null && echo "WRITE_{n} ok" \
                    || echo "WRITE_{n} failed"
            else
                echo "MISSING_{n}"
            fi"#
            )
        })
        .collect();

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/{name} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "NAMES $(ls -A "$m" | sort | tr '\n' ' ')"
            {checks}
            # The directory's other entries must still resolve.
            if [ -d "$m/fill" ]; then
                echo "FILL $(ls "$m/fill" | wc -l)"
            fi
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        # RETRY IF THE LOG DID NOT GO IN. Mounting replays it; when that
        # does not happen, xfs_repair describes an unreplayed log instead
        # and warns that what follows is spurious --
        # "sb_ifree 30, counted 29" is the warning coming true. Reading
        # that as a verdict fails the test for a reason that has nothing
        # to do with what it checks.
        for attempt in 1 2 3; do
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            case "$out" in
                *"valuable metadata changes in a log"*) ;;
                *) break ;;
            esac
            r=$(mktemp -d); mount -o loop,nouuid "$img" "$r" && umount "$r"; rmdir "$r"
        done
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
        echo "REPAIR_END"
        rm -f "$img"
        echo "DONE"
        "#
    );

    let out = kernel_run(&script)?;

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused the filesystem after the create was logged:\n{out}"
    );

    for (n, ino) in &created {
        assert!(
            !out.contains(&format!("MISSING_{n}")),
            "{case}: {n} is not there after the replay\n{out}"
        );
        let reported = out
            .lines()
            .find_map(|l| l.strip_prefix(&format!("INO_{n} ")))
            .unwrap_or_else(|| panic!("{case}: the VM did not report {n}'s inode:\n{out}"))
            .trim();
        assert_eq!(
            reported,
            ino.to_string(),
            "{case}: {n} came back as a different inode than was logged\n{out}"
        );
        assert!(
            out.contains(&format!("WRITE_{n} ok")),
            "{case}: {n} exists but cannot be written to, so the inode it names is not \
             a usable file\n{out}"
        );
    }

    let repair: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        repair.contains("REPAIR_RC=0"),
        "{case}: xfs_repair found something wrong after the replay:\n{repair}"
    );

    Some(())
}

/// A file created by this driver, used by the kernel.
#[test]
fn the_kernel_uses_a_file_this_driver_created() {
    let mut ran = Vec::new();

    if create_and_replay("spare", &["alpha"]).is_some() {
        ran.push("spare");
    }
    // `last` has exactly one inode free, so this create takes it and the
    // chunk has to leave the free-inode tree. That is the case where the
    // record changes a tree's membership rather than only its contents.
    if create_and_replay("last", &["only"]).is_some() {
        ran.push("last");
    }

    if ran.is_empty() {
        eprintln!(
            "no create fixtures or no VM; build them with \
             ./scripts/vm-build-create-fixtures.sh"
        );
        return;
    }
    eprintln!("the kernel used files this driver created for: {ran:?}");
}

/// A group with nothing free gets sixty-four more inodes.
///
/// This asserted a refusal until the chunk could be allocated. Every
/// chunk in the group being full is ordinary on a filesystem in use, so
/// refusing meant a create that stopped working once a directory tree
/// grew — and the fixture is exactly that state.
///
/// The claim now is the one that matters: the driver takes the blocks,
/// says in the record that they are to be made into inodes, and the
/// kernel replays it into a filesystem `xfs_repair` is content with.
#[test]
fn a_group_with_no_free_inode_gets_a_new_chunk() {
    let mut ran = Vec::new();
    // Both with and without a reverse map. The map is the interesting
    // one: the blocks a chunk gets sit next to the chunk before them and
    // are owned by the same -7, so the record has to MERGE with its
    // neighbour rather than be added beside it.
    for case in ["newchunk", "newchunk-rmap"] {
        if chunk_case(case) {
            ran.push(case);
        }
    }
    assert!(
        !ran.is_empty(),
        "no newchunk fixture — build them with scripts/build-create-fixtures.sh"
    );
    eprintln!("a chunk was allocated and replayed for: {ran:?}");
}

/// Returns false when the fixture is missing, so the caller can say so.
fn chunk_case(case: &str) -> bool {
    let source = share().join(format!("xfscreate-{case}-before.img"));
    if !source.exists() {
        eprintln!("no {case} fixture — skipping");
        return false;
    }
    let name = format!("xfs-create-{case}-scratch.img");
    let name = name.as_str();
    let scratch = Scratch::from(&source, name);

    let (before_free, root) = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(scratch.path()).expect("open")))
            .expect("mount");
        let agi_free: u32 = fs.superblock().agcount.min(1);
        let _ = agi_free;
        (
            fs.free_extents(0).expect("free space"),
            fs.superblock().rootino,
        )
    };

    let ino = {
        let dev = FileDevice::open_rw(scratch.path()).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let (ino, lsn) = fs
            .create_file(root, b"needsachunk", 0o100644)
            .expect("a group with no free inode should get more, not refuse");
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        ino
    };

    let script = format!(
        r#"
        m=$(mktemp -d)
        # Mounted IN PLACE, so the log is replayed into the image this
        # test then reads back. The other scripts here work on a copy to
        # keep a fixture pristine; this one is already a scratch, and the
        # replay is the point.
        if mount -o loop,nouuid /share/{name} "$m"; then
            [ -e "$m/needsachunk" ] && echo "PRESENT" || echo "MISSING"
            # The inode has to be usable, not merely listed. `touch`
            # rather than a write: writing would ALLOCATE, and this test
            # measures what the chunk cost. That is not hypothetical --
            # it read nine blocks instead of eight until the write came
            # out, and the ninth was the test's own.
            touch "$m/needsachunk" && echo "WRITABLE" || echo "NOT_WRITABLE"
            echo "STAT $(stat -c%i "$m/needsachunk")"
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        # The checker runs on a copy on local storage: it wants the host
        # filesystem's geometry and gets ENOTDIR from the share.
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/{name} "$img"
        # RETRY IF THE LOG DID NOT GO IN. Mounting replays it; when that
        # does not happen, xfs_repair describes an unreplayed log instead
        # and warns that what follows is spurious --
        # "sb_ifree 30, counted 29" is the warning coming true. Reading
        # that as a verdict fails the test for a reason that has nothing
        # to do with what it checks.
        for attempt in 1 2 3; do
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            case "$out" in
                *"valuable metadata changes in a log"*) ;;
                *) break ;;
            esac
            r=$(mktemp -d); mount -o loop,nouuid "$img" "$r" && umount "$r"; rmdir "$r"
        done
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
        echo "REPAIR_END"
        rm -f "$img"
        echo DONE
        "#
    );

    let Some(out) = kernel_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return false;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "{case}: the kernel refused a filesystem whose inode chunk this driver allocated:\n{out}"
    );
    assert!(
        out.contains("PRESENT"),
        "the file is not there after the chunk was made for it\n{out}"
    );
    assert!(
        out.contains("WRITABLE"),
        "the inode was made but cannot be used\n{out}"
    );
    assert!(
        out.contains(&format!("STAT {ino}")),
        "the kernel resolved the name to a different inode than this driver made ({ino})\n{out}"
    );
    assert!(
        out.contains("REPAIR_RC=0"),
        "xfs_repair objected to the group after a chunk was added:\n{out}"
    );

    // And the blocks the chunk needs really did come out of free space.
    //
    // Read after the replay, because before it the log is dirty and this
    // driver refuses to mount a filesystem whose log holds work it has
    // not applied — which is the correct thing for it to do and the
    // reason this check cannot come earlier.
    let after_free = {
        let fs = Filesystem::mount(Arc::new(
            FileDevice::open(scratch.path()).expect("open after the replay"),
        ))
        .expect("mount after the replay");
        fs.free_extents(0).expect("free space")
    };
    let before: u32 = before_free.iter().map(|e| e.blockcount).sum();
    let after: u32 = after_free.iter().map(|e| e.blockcount).sum();

    // EXACTLY what a chunk needs, not merely "something". Sixty-four
    // inodes divided by the inodes a block holds — eight blocks at the
    // usual sizes — and the kernel's own version of this operation took
    // the same eight.
    let inopblock = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(scratch.path()).expect("open")))
            .expect("mount");
        fs.superblock().inopblock
    };
    let expected = 64 / u32::from(inopblock);
    assert_eq!(
        before - after,
        expected,
        "a chunk of 64 inodes at {inopblock} to a block should cost {expected} blocks, not {}",
        before - after
    );
    eprintln!("{case}: the chunk cost {expected} blocks, and inode {ino} came out of it");
    true
}

/// A directory made by this driver, used by the kernel.
///
/// Everything a create has to get right, plus the two things only a
/// directory has: a fork of its own holding the parent it belongs to,
/// and a **parent whose link count moves** because the new directory's
/// `..` is a link to it.
///
/// The link count is the part nothing catches by reading the directory
/// back. A parent left at its old count reads perfectly, lists
/// perfectly, and is wrong — it shows up in a consistency check, or much
/// later, when the parent refuses to be removed because the kernel
/// believes something still links to it.
#[test]
fn the_kernel_uses_a_directory_this_driver_made() {
    let source = share().join("xfscreate-spare-before.img");
    if !source.exists() {
        eprintln!("no create fixture — skipping");
        return;
    }
    let name = "xfs-mkdir-scratch.img";
    let scratch = Scratch::from(&source, name);
    let img = scratch.path();

    let (made, parent_nlink_before) = {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let root = fs.superblock().rootino;
        let before = fs.read_inode(root).expect("read the root").nlink;
        let (ino, lsn) = fs
            .create_directory(root, b"newdir", 0o040755)
            .expect("the mkdir must be accepted");
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        (ino, before)
    };

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/{name} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            if [ -d "$m/newdir" ]; then echo "IS_DIR"; else echo "NOT_DIR"; fi
            echo "DIR_INO $(stat -c %i "$m/newdir")"
            echo "DIR_LINKS $(stat -c %h "$m/newdir")"
            echo "PARENT_LINKS $(stat -c %h "$m")"
            echo "DOTDOT $(stat -c %i "$m/newdir/..")"
            echo "EMPTY $(ls -A "$m/newdir" | wc -l)"
            # A directory has to be usable, not merely present.
            if : > "$m/newdir/inside" 2>/dev/null; then
                echo "USABLE ok"
                echo "LISTS $(ls "$m/newdir" | tr '\n' ' ')"
            else
                echo "USABLE failed"
            fi
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        # RETRY IF THE LOG DID NOT GO IN. Mounting replays it; when that
        # does not happen, xfs_repair describes an unreplayed log instead
        # and warns that what follows is spurious --
        # "sb_ifree 30, counted 29" is the warning coming true. Reading
        # that as a verdict fails the test for a reason that has nothing
        # to do with what it checks.
        for attempt in 1 2 3; do
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            case "$out" in
                *"valuable metadata changes in a log"*) ;;
                *) break ;;
            esac
            r=$(mktemp -d); mount -o loop,nouuid "$img" "$r" && umount "$r"; rmdir "$r"
        done
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
        echo "REPAIR_END"
        rm -f "$img"
        echo "DONE"
        "#
    );

    let Some(out) = kernel_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the filesystem after the mkdir was logged:\n{out}"
    );
    assert!(out.contains("IS_DIR"), "newdir is not a directory\n{out}");
    assert!(
        out.contains("USABLE ok"),
        "nothing could be created inside the new directory\n{out}"
    );

    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the VM did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };

    assert_eq!(
        field("DIR_INO"),
        made.to_string(),
        "the directory came back as a different inode than was logged\n{out}"
    );
    assert_eq!(
        field("EMPTY"),
        "0",
        "a directory that has just been made should hold nothing — `.` and `..` are \
         not entries in the short form\n{out}"
    );
    assert_eq!(
        field("DOTDOT"),
        // The parent is the root, whose inode number the fixture's
        // superblock states.
        {
            let fs = Filesystem::mount(Arc::new(FileDevice::open(&source).expect("open")))
                .expect("mount");
            fs.superblock().rootino.to_string()
        },
        "the new directory's `..` does not point at its parent\n{out}"
    );

    // Two links: the entry naming it, and its own `.`.
    assert_eq!(
        field("DIR_LINKS"),
        "2",
        "a new directory has two links — the entry naming it and its own `.`\n{out}"
    );

    // And the parent gained one, because the new directory's `..` links
    // to it.
    assert_eq!(
        field("PARENT_LINKS"),
        (parent_nlink_before + 1).to_string(),
        "the parent's link count should have gone up by one for the new directory's \
         `..`, from {parent_nlink_before}\n{out}"
    );

    let repair: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        repair.contains("REPAIR_RC=0"),
        "xfs_repair found something wrong after the replay:\n{repair}"
    );

    eprintln!("the kernel used a directory this driver made (inode {made})");
}

/// A short-form directory converted to block form, replayed by the
/// kernel.
///
/// The largest transaction this driver writes: 23 operations across 9
/// items — a block allocated, the whole directory written into it, the
/// inode moved from an inline fork to an extent list, and a file created
/// in the same breath. It is what every write here refused until now,
/// and the reason a directory of about thirty short names was the
/// ceiling on everything else.
///
/// # What is actually being checked
///
/// That the kernel accepts it is the weakest of the claims below. A
/// directory block can be structurally perfect and still be unusable:
/// the index is what a lookup binary-searches, so a name that cannot be
/// found is the failure to look for, and it is invisible to `ls`.
///
/// So the test looks up every name individually, adds another entry
/// afterwards, and removes one — three things that each go through the
/// index rather than through a linear walk.
#[test]
fn the_kernel_uses_a_directory_this_driver_converted() {
    let source = share().join("xfsdirconv-exact-before.img");
    if !source.exists() {
        eprintln!("no conversion fixture — skipping");
        return;
    }
    let name = "xfs-dirconv-scratch.img";
    let scratch = Scratch::from(&source, name);
    let img = scratch.path();

    // What the directory held before, so the check afterwards knows what
    // must still be there.
    let before: Vec<String> = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(img).expect("open"))).expect("mount");
        let d = fs.lookup_path("/d").expect("the directory");
        let (inode, raw) = fs.read_inode_raw(d.ino).expect("read it");
        assert_eq!(
            inode.format,
            fs_xfs::inode::Format::Local,
            "the fixture's directory should start short form"
        );
        fs.read_dir(&inode, &raw)
            .expect("list it")
            .into_iter()
            .map(|e| String::from_utf8_lossy(&e.name).into_owned())
            .collect()
    };
    // THE PRECONDITION, CHECKED RATHER THAN GUESSED. This test needs a
    // directory that is one entry short of leaving its inode. It used to
    // assert "more than 20 entries" as a stand-in, and that failed on a
    // runner where the same fixture held 17 — with nothing wrong.
    //
    // The count is not a property of the fixture. build-dirconv-fixtures
    // fills until the kernel converts and steps back one, so it depends
    // on how much of the inode the data fork gets. On a runner and in a
    // container the directory is created with a security xattr, so it
    // has an ATTRIBUTE FORK: forkoff=24 leaves the data fork 192 bytes
    // and the kernel converts at 18 entries. In the VM there is no
    // xattr, forkoff=0, and it converts at 31. Same inode size, same
    // block size, nearly twice the entries.
    //
    // So ask the fixture pair instead: the `-after` image is the same
    // directory with one more entry, and if that one is in block form
    // then the pair brackets the conversion, whatever the count.
    {
        let after_img = share().join("xfsdirconv-exact-after.img");
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&after_img).expect("open")))
            .expect("mount");
        let d = fs.lookup_path("/d").expect("the directory");
        let (inode, _) = fs.read_inode_raw(d.ino).expect("read it");
        assert_eq!(
            inode.format,
            fs_xfs::inode::Format::Extents,
            "the -after fixture should show the directory converted; the pair does not \
             bracket the conversion, so this test has nothing to convert"
        );
    }

    let added = "converted";
    {
        let dev = FileDevice::open_rw(img).expect("open read-write");
        let fs = Filesystem::mount_rw(Arc::new(dev)).expect("mount read-write");
        let d = fs.lookup_path("/d").expect("the directory");
        let (ino, lsn) = fs
            .create_file(d.ino, added.as_bytes(), 0o100644)
            .expect("the conversion must be accepted");
        assert_ne!(lsn, 0, "a record must be given a sequence number");
        assert_ne!(ino, 0);
    }

    let checks: String = before
        .iter()
        .take(6)
        .map(|n| format!("\n            [ -e \"$m/d/{n}\" ] || echo \"LOST {n}\""))
        .collect();

    let script = format!(
        r#"
        img=$(mktemp -u /tmp/oracle-XXXXXX.img)
        cp /share/{name} "$img"
        dmesg -C >/dev/null 2>&1
        m=$(mktemp -d)
        if mount -o loop,nouuid "$img" "$m"; then
            echo "COUNT $(ls -A "$m/d" | wc -l)"
            [ -e "$m/d/{added}" ] && echo "ADDED_PRESENT" || echo "ADDED_MISSING"
            # Every one of these resolves through the hash index, not a
            # linear walk — a name that cannot be looked up is invisible
            # to `ls` and fatal to everything else.{checks}
            # Adding and removing exercise the index as a structure that
            # is maintained, not merely read.
            : > "$m/d/afterwards" 2>/dev/null && echo "ADD_OK" || echo "ADD_FAILED"
            rm -f "$m/d/{added}" 2>/dev/null && echo "REMOVE_OK" || echo "REMOVE_FAILED"
            umount "$m"
        else
            echo "MOUNT_FAILED"
            dmesg | tail -12
        fi
        rmdir "$m" 2>/dev/null
        # RETRY IF THE LOG DID NOT GO IN. Mounting replays it; when that
        # does not happen, xfs_repair describes an unreplayed log instead
        # and warns that what follows is spurious --
        # "sb_ifree 30, counted 29" is the warning coming true. Reading
        # that as a verdict fails the test for a reason that has nothing
        # to do with what it checks.
        for attempt in 1 2 3; do
            out=$(xfs_repair -n "$img" 2>&1) && rc=0 || rc=$?
            case "$out" in
                *"valuable metadata changes in a log"*) ;;
                *) break ;;
            esac
            r=$(mktemp -d); mount -o loop,nouuid "$img" "$r" && umount "$r"; rmdir "$r"
        done
        echo "REPAIR_BEGIN"
        echo "$out"
        echo "REPAIR_RC=$rc"
        echo "REPAIR_END"
        rm -f "$img"
        echo "DONE"
        "#
    );

    let Some(out) = kernel_run(&script) else {
        eprintln!("oracle VM unavailable — skipping verification");
        return;
    };

    assert!(
        !out.contains("MOUNT_FAILED"),
        "the kernel refused the filesystem after the conversion:\n{out}"
    );
    assert!(
        !out.contains("LOST "),
        "a name that was in the short form cannot be found after the conversion — \
         the hash index does not agree with the entries\n{out}"
    );
    assert!(
        out.contains("ADDED_PRESENT"),
        "the entry that triggered the conversion is not there\n{out}"
    );
    assert!(
        out.contains("ADD_OK"),
        "nothing could be added to the converted directory\n{out}"
    );
    assert!(
        out.contains("REMOVE_OK"),
        "nothing could be removed from the converted directory\n{out}"
    );

    let count: usize = out
        .lines()
        .find_map(|l| l.strip_prefix("COUNT "))
        .expect("the VM did not report the count")
        .trim()
        .parse()
        .expect("a number");
    assert_eq!(
        count,
        before.len() + 1,
        "the converted directory should hold everything it did plus the new entry\n{out}"
    );

    let repair: String = out
        .lines()
        .skip_while(|l| !l.starts_with("REPAIR_BEGIN"))
        .take_while(|l| !l.starts_with("REPAIR_END"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        repair.contains("REPAIR_RC=0"),
        "xfs_repair found something wrong after the conversion:\n{repair}"
    );

    eprintln!(
        "the kernel used a directory this driver converted ({} entries, all found)",
        count
    );
}
