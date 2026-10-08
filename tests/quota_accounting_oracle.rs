//! Quota usage, hard-limit refusal, and replay must agree with Linux XFS.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use sha2::{Digest, Sha256};
use std::io::Read;
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
        r##"
        m=$(mktemp -d)
        if ! mount -o loop,nouuid,uquota {image} "$m"; then
            echo MOUNT_FAILED
            dmesg | tail -12
            exit 0
        fi
        report() {{
            id=$1
            # Numeric report IDs have a leading '#' (xfsprogs report_row).
            blocks=$(xfs_quota -x -c 'report -u -b -n' "$m" | awk -v id="$id" '$1 == "#" id {{print $2; exit}}')
            inodes=$(xfs_quota -x -c 'report -u -i -n' "$m" | awk -v id="$id" '$1 == "#" id {{print $2; exit}}')
            echo "USAGE_${{id}} $blocks $inodes"
        }}
        report 0
        report 65534
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "##
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

// xfs_quota reports KiB; xfs_db exposes the filesystem-block counters
// and limits actually stored in each Linux-created dquot.
fn expect_block_units(image: &str, blocksize: u32, blocks: u64, limited: &str) {
    let out = kernel_run(&format!(
        r##"
        m=$(mktemp -d)
        mount -o loop,nouuid,uquota,gquota,pquota {image} "$m"
        for owner in u:65534 g:65534 p:7; do
            kind=${{owner%:*}}
            id=${{owner#*:}}
            xfs_quota -x -c "report -$kind -b -n" "$m" |
                awk -v kind="$kind" -v id="$id" '$1 == "#" id {{print "REPORT", kind, $2, $3, $4}}'
        done
        umount "$m"
        rmdir "$m"
        for owner in u:65534 g:65534 p:7; do
            kind=${{owner%:*}}
            id=${{owner#*:}}
            echo "DQUOT $kind"
            xfs_db -r -c "dquot -$kind $id" \
                -c 'p diskdq.bcount diskdq.blk_softlimit diskdq.blk_hardlimit' {image}
        done
        echo DONE
        "##
    ));
    for kind in ["u", "g", "p"] {
        let limit = u64::from(kind == limited);
        let kib = u64::from(blocksize) / 1024;
        let report = format!(
            "REPORT {kind} {} {} {}",
            blocks * kib,
            limit * kib,
            limit * kib
        );
        assert!(out.lines().any(|line| line.trim() == report), "{out}");
        let record = out
            .split(&format!("DQUOT {kind}\n"))
            .nth(1)
            .expect("dquot output");
        let record = record.split("DQUOT ").next().unwrap();
        for field in [
            format!("diskdq.bcount = {blocks}"),
            format!("diskdq.blk_softlimit = {limit}"),
            format!("diskdq.blk_hardlimit = {limit}"),
        ] {
            assert!(record.lines().any(|line| line.trim() == field), "{out}");
        }
    }
}

fn image_digest(path: &str) -> [u8; 32] {
    let mut file = std::fs::File::open(path).unwrap();
    let mut digest = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            return digest.finalize().into();
        }
        digest.update(&buffer[..count]);
    }
}

#[test]
fn filesystem_block_quota_boundaries_match_linux_for_every_owner_and_geometry() {
    assert!(
        share().is_dir(),
        "`chore fixtures` must create the VM share"
    );
    for blocksize in [1024, 2048, 4096] {
        for (kind, id) in [("u", 65534), ("g", 65534), ("p", 7)] {
            let scratch = scratch::Volume::empty(
                SUITE,
                &format!("units-{}-{blocksize}-{kind}.img", std::process::id()),
                320 * 1024 * 1024,
            );
            let path = scratch.path().to_string_lossy().into_owned();
            let image = scratch.guest();
            let setup = kernel_run(&format!(
                r#"
                mkfs.xfs -q -f -b size={blocksize} -m rmapbt=0 {image}
                m=$(mktemp -d)
                mount -o loop,nouuid,uquota,gquota,pquota {image} "$m"
                touch "$m/boundary"
                chown 65534:65534 "$m/boundary"
                xfs_io -c 'chproj 7' "$m/boundary"
                xfs_quota -x -c 'limit -{kind} bsoft={blocksize} bhard={blocksize} {id}' "$m"
                umount "$m"
                rmdir "$m"
                echo DONE
                "#
            ));
            assert!(setup.contains("DONE"), "{setup}");
            expect_block_units(&image, blocksize, 0, kind);
            let ino = stage(&path, |fs| {
                assert_eq!(fs.superblock().blocksize, blocksize);
                let ino = fs.lookup_path("/boundary").unwrap().ino;
                fs.write_into_empty_file(ino, &vec![0x5a; blocksize as usize])
                    .expect("one filesystem block must fit the Linux one-block hard limit");
                ino
            });
            expect_block_units(&image, blocksize, 1, kind);
            stage(&path, |fs| fs.truncate_to_zero(ino).unwrap());
            expect_block_units(&image, blocksize, 0, kind);

            // Refusal must leave the entire image unchanged, including data,
            // allocator metadata, quota records, and the journal.
            let before = image_digest(&path);
            stage(&path, |fs| {
                let error = fs
                    .write_into_empty_file(ino, &vec![0x5a; 2 * blocksize as usize])
                    .expect_err("two filesystem blocks exceed the one-block hard limit");
                assert!(error.to_string().contains("hard limit"), "{error}");
                assert_eq!(fs.read_inode(ino).unwrap().size, 0);
            });
            assert_eq!(
                image_digest(&path),
                before,
                "refused {kind} quota write changed the image"
            );
            expect_block_units(&image, blocksize, 0, kind);
            let out = kernel_run(&format!(
                "echo REPAIR_BEGIN\nxfs_repair -n {image} 2>&1 && echo REPAIR_RC=0 || echo REPAIR_RC=$?\necho REPAIR_END\necho DONE"
            ));
            repair::assert_agreed(&out, "the quota boundary volume");
        }
    }
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
        # Private scratch parents block nobody, and creation can reserve over 4 KiB.
        # Create and transfer ownership as root before imposing the hard limit.
        touch "$m/over-limit"
        chown 65534:65534 "$m/over-limit"
        xfs_quota -x -c 'limit -u bhard=4k 65534' "$m"
        if ! umount "$m"; then sleep 2; umount "$m" || echo UMOUNT_FAILED; fi
        rmdir "$m"
        echo DONE
        "#
    ));
    assert!(
        setup.contains("DONE"),
        "quota fixture setup failed:\n{setup}"
    );

    // Ask the independent debugger to find the actual Linux-created
    // record before testing enforcement. Quota inode di_size does not
    // describe the addressable sparse dquot clusters.
    let fs = Filesystem::mount(Arc::new(FileDevice::open(&path).unwrap())).unwrap();
    let sb = fs.superblock();
    let (qfile, qraw) = fs.read_inode_raw(sb.uquotino).unwrap();
    let per_block = u64::from(sb.blocksize) / 136;
    let logical = 65534 / per_block;
    assert!(logical * u64::from(sb.blocksize) >= qfile.size);
    let extents = fs.data_extents(&qfile, &qraw).unwrap();
    assert!(fs_xfs::extent::lookup(&extents, logical).is_some());
    let reference = kernel_run(&format!(
        "xfs_db -r -c 'inode {}' -c 'p core.size' -c 'dquot -u 65534' \
         -c 'p diskdq.id diskdq.blk_hardlimit diskdq.bcount diskdq.icount' {image}\necho DONE",
        sb.uquotino,
    ));
    for field in [
        format!("core.size = {}", qfile.size),
        "diskdq.id = 65534".into(),
        // On-disk limits use filesystem blocks (Linux xfs_qm_scall_setqlim).
        format!("diskdq.blk_hardlimit = {}", 4096 / sb.blocksize),
        "diskdq.bcount = 0".into(),
        "diskdq.icount = 1".into(),
    ] {
        assert!(
            reference.lines().any(|line| line.trim() == field),
            "{reference}"
        );
    }
    let fsblock_kib = u64::from(sb.blocksize) / 1024;
    drop(fs);

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
            let name = format!("entry_{index:02}_{}", "x".repeat(40));
            fs.create_file(directory, name.as_bytes(), 0o100644)
                .expect("populate the quota-accounted directory");
        }
        let blocks = fs.read_inode(directory).expect("directory inode").nblocks;
        assert!(
            blocks > 0,
            "directory growth must allocate quota-accounted blocks"
        );
        (root_file, blocks)
    });
    let create_blocks = baseline.0.blocks + directory_blocks * fsblock_kib;
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
