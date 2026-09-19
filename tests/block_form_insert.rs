//! A directory keeps taking entries after it has outgrown the inode (#215).
//!
//! A short-form directory that will not hold one more name is moved into a
//! block of its own, and that much worked. What did not is the next entry:
//! adding to a directory already in block form was refused, so a directory
//! took about two dozen names and then nothing — which is what a probe of
//! this driver hit long before it ran out of log.
//!
//! Here one mount fills a directory well past that point, and the kernel
//! replays the records and lists what it finds. Every name has to be there,
//! each resolving to the inode the driver said, and `xfs_repair` has to
//! accept the volume.
//!
//! Skips when no kernel is reachable (see `common::transport`); ci-test.sh
//! turns that skip into a failure in CI.

mod common;

use common::{kernel_run, repair, scratch, share};

/// Where this suite's scratch volume lives, under
/// `.vm-share/scratch/`, out of reach of the suites that scan the
/// fixtures beside them (#223).
const SUITE: &str = "block_form_insert";
use fs_core::{BlockDevice, FileDevice};
use fs_xfs::Filesystem;
use std::sync::Arc;

/// More names than one directory block can hold, so the run ends at the
/// block's capacity rather than at a number chosen here. Twelve or so fill
/// the inode; a 4 KiB block takes about 124 of this length.
const TRIES: u32 = 300;

/// Past the inode by a margin, so a run that stopped early is a failure
/// rather than a smaller pass.
const AT_LEAST: usize = 100;

#[test]
fn a_directory_takes_entries_after_it_leaves_the_inode() {
    // No fixture directory means no fixture set here; the job that runs
    // this builds them first.
    if !share().exists() {
        eprintln!("no .vm-share — skipped");
        return;
    }
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        320 * 1024 * 1024,
    );
    let image = scratch.path().to_path_buf();
    let name = scratch.guest();
    let Some(built) = kernel_run(&format!(
        "mkfs.xfs -q -f {name} 2>&1 && echo MKFS_OK; echo DONE"
    )) else {
        eprintln!("no kernel reachable (fixture or VM unavailable) — skipped");
        return;
    };
    assert!(built.contains("MKFS_OK"), "mkfs.xfs failed:\n{built}");
    let path = image.to_str().unwrap().to_string();

    let mut made: Vec<(String, u64)> = Vec::new();
    {
        let dev = Arc::new(FileDevice::open_rw(&path).unwrap());
        let fs = Filesystem::mount_rw(dev as Arc<dyn BlockDevice>).expect("mount_rw");
        let root = fs.lookup_path("/").unwrap().ino;
        let (dir, _) = fs
            .create_directory(root, b"d", 0o40755)
            .expect("the directory to fill");
        for i in 0..TRIES {
            let file = format!("entry_{i:04}");
            match fs.create_file(dir, file.as_bytes(), 0o100644) {
                Ok((ino, _)) => made.push((file, ino)),
                // THE BLOCK FILLING IS NOT A FAILURE. What this pins is
                // that it is refused cleanly, naming what is missing, and
                // that the directory is still exactly what it was — the
                // kernel checks that below.
                Err(e) => {
                    let said = e.to_string();
                    assert!(
                        said.contains("leaf-form"),
                        "the refusal should name what is not implemented: {said}"
                    );
                    break;
                }
            }
        }
        assert!(
            made.len() >= AT_LEAST,
            "only {} entries went in, which is not past the inode by enough to say the \
             block-form insert works",
            made.len()
        );
        // The directory has left the inode: its fork is an extent now.
        let (inode, _) = fs.read_inode_raw(dir).expect("the directory");
        assert_ne!(
            inode.format,
            fs_xfs::inode::Format::Local,
            "the directory never left the inode, so this tests nothing"
        );
    }

    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if mount -o loop,nouuid {name} "$m"; then
            echo "COUNT $(ls "$m/d" | wc -l)"
            echo "FIRST $(stat -c %i "$m/d/entry_0000")"
            echo "LAST $(stat -c %i "$m/d/$(ls "$m/d" | sort | tail -1)")"
            echo "NAMES $(ls "$m/d" | sort | tr '\n' ' ')"
            # RETRIED ONCE. A busy unmount under a loaded runner is
            # ordinary and clears in a moment; one that does not is the
            # failure worth reporting, because the kernel writes the
            # summary counters at unmount and nothing else does.
            if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
            echo MOUNTED
        else
            echo MOUNT_FAILED
            dmesg | tail -10
        fi
        rmdir "$m"
        echo "REPAIR_BEGIN"
        xfs_repair -n {name} 2>&1 && echo "REPAIR_RC=0" || echo "REPAIR_RC=$?"
        echo "REPAIR_END"
        echo DONE
        "#
    ))
    .expect("kernel");

    assert!(
        out.contains("MOUNTED"),
        "the kernel refused the volume:\n{out}"
    );
    repair::assert_agreed(&out, "the volume after the directory was filled");
    let field = |key: &str| -> String {
        out.lines()
            .find_map(|l| l.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| panic!("the guest did not report {key}:\n{out}"))
            .trim()
            .to_string()
    };
    assert_eq!(
        field("COUNT"),
        made.len().to_string(),
        "the directory holds exactly the names that went in"
    );
    assert_eq!(
        field("FIRST"),
        made[0].1.to_string(),
        "the first name resolves to the inode the driver gave it"
    );
    assert_eq!(
        field("LAST"),
        made[made.len() - 1].1.to_string(),
        "and so does the last"
    );
    // And the names themselves.
    let mut expected: Vec<String> = made.iter().map(|(n, _)| n.clone()).collect();
    expected.sort();
    assert_eq!(
        field("NAMES"),
        expected.join(" "),
        "the kernel lists exactly the names this driver created"
    );
}
