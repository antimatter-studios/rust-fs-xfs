//! Quota usage, hard-limit refusal, and replay must agree with Linux XFS.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::sync::Arc;

mod common;
use common::{kernel_run, repair, scratch, share};

const SUITE: &str = "quota_accounting_oracle";

#[derive(Debug, Clone, Copy)]
struct Usage {
    blocks: u64,
    inodes: u64,
}

fn usage(image: &str) -> (Usage, Usage) {
    let out = kernel_run(&format!(
        r#"
        m=$(mktemp -d)
        if ! mount -o loop,nouuid,uquota {image} "$m"; then
            echo MOUNT_FAILED
            dmesg | tail -12
            exit 0
        fi
        report() {{
            id=$1
            blocks=$(xfs_quota -x -c 'report -u -b -n' "$m" | awk -v id="$id" '$1 == id {{print $2; exit}}')
            inodes=$(xfs_quota -x -c 'report -u -i -n' "$m" | awk -v id="$id" '$1 == id {{print $2; exit}}')
            echo "USAGE_${{id}} $blocks $inodes"
        }}
        report 0
        report 65534
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        !out.contains("MOUNT_FAILED"),
        "Linux could not replay and mount the quota volume:\n{out}"
    );
    let parse = |id: &str| {
        let line = out
            .lines()
            .find(|line| line.starts_with(&format!("USAGE_{id} ")))
            .unwrap_or_else(|| panic!("the kernel did not report quota {id}:\n{out}"));
        let fields: Vec<_> = line.split_whitespace().collect();
        assert_eq!(fields.len(), 3, "malformed quota usage line: {line}");
        Usage {
            blocks: fields[1]
                .parse()
                .unwrap_or_else(|_| panic!("bad block usage in {line}")),
            inodes: fields[2]
                .parse()
                .unwrap_or_else(|_| panic!("bad inode usage in {line}")),
        }
    };
    (parse("0"), parse("65534"))
}

fn stage<T>(image: &str, operation: impl FnOnce(&Filesystem) -> T) -> T {
    let fs = Filesystem::mount_rw(Arc::new(
        FileDevice::open_rw(image).expect("open quota image read-write"),
    ))
    .expect("mount quota image read-write");
    operation(&fs)
}

fn expect_usage(got: Usage, expected: Usage, who: &str, action: &str) {
    assert_eq!(
        got.blocks, expected.blocks,
        "{who} block quota after {action}"
    );
    assert_eq!(
        got.inodes, expected.inodes,
        "{who} inode quota after {action}"
    );
}

#[test]
fn quotas_track_create_write_truncate_unlink_and_refuse_over_limit_write() {
    assert!(
        share().is_dir(),
        "`chore fixtures` must create the VM share"
    );
    let scratch = scratch::Volume::empty(
        SUITE,
        &format!("{}.img", std::process::id()),
        320 * 1024 * 1024,
    );
    let path = scratch.path().to_string_lossy().into_owned();
    let image = scratch.guest();

    let setup = kernel_run(&format!(
        r#"
        mkfs.xfs -q -f -m rmapbt=0 {image}
        m=$(mktemp -d)
        mount -o loop,nouuid,uquota {image} "$m"
        xfs_quota -x -c 'limit -u bhard=4k 65534' "$m"
        chmod 0777 "$m"
        runuser -u nobody -- touch "$m/over-limit"
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        setup.contains("DONE"),
        "quota fixture setup failed:\n{setup}"
    );

    let baseline = usage(&image);
    stage(&path, |fs| {
        let victim = fs.lookup_path("/over-limit").expect("nobody's empty file");
        assert_eq!(victim.uid, 65534, "the hard limit is on nobody's quota");
        let err = fs
            .write_into_empty_file(victim.ino, &[0x5a; 8192])
            .expect_err("a write beyond the user's hard limit must be refused");
        assert!(err.to_string().contains("hard limit"), "{err}");
        let still_empty = fs
            .read_inode(victim.ino)
            .expect("the victim stays readable");
        assert_eq!(
            still_empty.size, 0,
            "the refused write has no partial inode update"
        );
    });
    let after_refusal = usage(&image);
    expect_usage(
        after_refusal.1,
        baseline.1,
        "nobody",
        "the refused over-limit write",
    );

    let (root_file, directory_blocks) = stage(&path, |fs| {
        let root = fs.root_inode().expect("root inode").ino;
        let root_file = fs
            .create_file(root, b"accounted", 0o100644)
            .expect("create an empty file")
            .0;
        let directory = fs
            .create_directory(root, b"quota-dir", 0o40755)
            .expect("create a quota-accounted directory")
            .0;
        for index in 0..16 {
            let name = format!("entry_{index:02}");
            fs.create_file(directory, name.as_bytes(), 0o100644)
                .expect("populate the quota-accounted directory");
        }
        let blocks = fs.read_inode(directory).expect("directory inode").nblocks;
        (root_file, blocks)
    });
    let create_blocks = baseline.0.blocks + directory_blocks * 4;
    let after_create = usage(&image);
    expect_usage(
        after_create.0,
        Usage {
            blocks: create_blocks,
            inodes: baseline.0.inodes + 18,
        },
        "root",
        "create replay",
    );

    stage(&path, |fs| {
        fs.write_into_empty_file(root_file, &[0xa5; 8192])
            .expect("write two filesystem blocks");
    });
    let after_write = usage(&image);
    expect_usage(
        after_write.0,
        Usage {
            blocks: create_blocks + 8,
            inodes: baseline.0.inodes + 18,
        },
        "root",
        "write replay",
    );

    stage(&path, |fs| {
        fs.truncate_to_zero(root_file).expect("truncate the file");
    });
    let after_truncate = usage(&image);
    expect_usage(
        after_truncate.0,
        Usage {
            blocks: create_blocks,
            inodes: baseline.0.inodes + 18,
        },
        "root",
        "truncate replay",
    );

    stage(&path, |fs| {
        let root = fs.root_inode().expect("root inode").ino;
        fs.unlink_file(root, b"accounted")
            .expect("unlink the empty file");
    });
    let final_usage = usage(&image);
    expect_usage(
        final_usage.0,
        Usage {
            blocks: create_blocks,
            inodes: baseline.0.inodes + 17,
        },
        "root",
        "unlink replay",
    );
    expect_usage(
        final_usage.1,
        baseline.1,
        "nobody",
        "all replayed operations",
    );

    let out = kernel_run(&format!(
        r#"
        echo REPAIR_BEGIN
        xfs_repair -n {image} 2>&1 && echo REPAIR_RC=0 || echo "REPAIR_RC=$?"
        echo REPAIR_END
        echo DONE
        "#
    ));
    repair::assert_agreed(&out, "the volume after quota accounting operations");
}
