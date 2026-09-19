//! A checkpoint too large for one in-core log buffer is split across
//! records (#216).
//!
//! One operation's record had to fit a single in-core buffer — 32 KiB on
//! an ordinary log — and anything larger was refused outright. The
//! kernel writes such a checkpoint as a sequence of records instead,
//! which is why it has no such limit.
//!
//! What reaches it is ordinary: a file interleaved with another, a block
//! at a time, so that freeing one leaves the other between every pair of
//! free blocks. The free space does not merge, three thousand separate
//! runs go back into the group, the free-space trees are laid out again
//! over dozens of blocks, and every one of them is logged. That is
//! 81,924 bytes against a 32,256-byte record.
//!
//! The kernel replays what this driver wrote and `xfs_repair` grades the
//! result: a record that was split wrongly — the wrong transaction id,
//! the wrong operation count, a boundary in the middle of an item — is a
//! checkpoint the kernel discards or applies in pieces, and either shows
//! up here.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// Where this suite's scratch volume lives (#223).
const SUITE: &str = "split_checkpoint";

/// One-block pieces of each file, interleaved. Enough that the free
/// space left behind cannot be described in one record.
const PIECES: u32 = 1500;

#[test]
fn a_checkpoint_larger_than_one_log_buffer_is_written_and_replayed() {
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
    let volume = scratch::Volume::empty(SUITE, "interleaved.img", 400 * 1024 * 1024);
    let image = volume.guest();

    let built = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -m rmapbt=1 {image} 2>&1 && echo MKFS_OK
        m=$(mktemp -d)
        mount -o loop {image} "$m" && echo MOUNT_OK
        echo "FREE_BEFORE $(df --output=avail -k "$m" | tail -1)"
        # Two files, a block each in turn, so neither leaves a run behind
        # when the other is freed.
        for i in $(seq 0 {last}); do
            dd if=/dev/zero of="$m/gone" bs=4096 count=1 seek=$((i * 2)) conv=notrunc status=none
            dd if=/dev/zero of="$m/kept" bs=4096 count=1 seek=$((i * 2 + 1)) conv=notrunc status=none
        done
        sync
        echo "GONE_EXTENTS $(xfs_bmap "$m/gone" | grep -c ':')"
        echo "KEPT_SUM $(md5sum < "$m/kept" | cut -d' ' -f1)"
        echo "KEPT_SIZE $(stat -c %s "$m/kept")"
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "#,
        last = PIECES - 1,
    ));
    assert!(
        built.contains("MKFS_OK") && built.contains("MOUNT_OK"),
        "building the volume failed:\n{built}"
    );
    let field = |out: &str, key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert!(
        field(&built, "GONE_EXTENTS").parse::<u32>().unwrap() > 1000,
        "the file is not fragmented enough for its truncate to need more than one record"
    );
    let kept_sum = field(&built, "KEPT_SUM");
    let kept_size = field(&built, "KEPT_SIZE");

    {
        let dev = Arc::new(FileDevice::open_rw(volume.path().to_str().unwrap()).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        let ino = fs.lookup_path("/gone").expect("the fragmented file").ino;
        fs.truncate_to_zero(ino)
            .expect("a truncate whose checkpoint does not fit one record");
        // AND SOMETHING AFTER IT, in the same mount. The records that
        // follow a split checkpoint name a tail, and recovery starts
        // there: if the tail names the split checkpoint's LAST record
        // rather than its first, recovery begins in the middle of it,
        // finds a transaction with no start, discards it — and the
        // truncate above is silently not applied.
        let root = fs.lookup_path("/").expect("the root").ino;
        fs.create_file(root, b"after", 0o100644)
            .expect("an operation after the split checkpoint");
    }

    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {image} "$m"; then
            echo "SIZE $(stat -c %s "$m/gone")"
            echo "BLOCKS $(stat -c %b "$m/gone")"
            echo "KEPT_SUM $(md5sum < "$m/kept" | cut -d' ' -f1)"
            echo "KEPT_SIZE $(stat -c %s "$m/kept")"
            echo "AFTER $([ -e "$m/after" ] && echo yes || echo no)"
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m"
        {repair}
        echo DONE
        "#,
        repair = repair::script(&image),
    ));

    assert!(
        out.contains("MOUNTED"),
        "the kernel refused the volume after the split checkpoint:\n{out}"
    );
    assert_eq!(field(&out, "SIZE"), "0", "the truncated file");
    assert_eq!(field(&out, "BLOCKS"), "0", "and what it still holds");
    // THE OTHER FILE IS UNTOUCHED. Its blocks sit between every pair the
    // truncate freed, so a record applied in the wrong place lands on
    // them rather than on free space.
    assert_eq!(
        field(&out, "KEPT_SIZE"),
        kept_size,
        "the neighbouring file changed length:\n{out}"
    );
    assert_eq!(
        field(&out, "KEPT_SUM"),
        kept_sum,
        "the neighbouring file's contents changed, which is what a record applied to \
         the wrong blocks looks like:\n{out}"
    );
    assert_eq!(
        field(&out, "AFTER"),
        "yes",
        "the operation after the split checkpoint is missing, so the records that \
         followed it were not replayed:\n{out}"
    );
    repair::assert_agreed(&out, "the volume after a checkpoint spanning records");
}
