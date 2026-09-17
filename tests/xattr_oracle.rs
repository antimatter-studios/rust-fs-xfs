//! Extended attributes read back as `xfs_db` wrote them (#91).
//!
//! `mkfs.xfs -p` makes the files and `xfs_db -x attr_set` gives each a set
//! of attributes shaped to land in one attribute-fork format: short form
//! inline in the inode, a single leaf block, a node B-tree over leaves, and
//! a leaf holding remote values over several blocks. `attr_set` fills a value
//! with `v`s to the length asked, and `xfs_db` itself reports which shape
//! each fork took, so both the answer and the shape come from the reference
//! tool. `xfs_repair -n` must accept the image first. Skips when xfsprogs is
//! not installed.

use fs_core::FileDevice;
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::process::Command;
use std::sync::Arc;

fn xfsprogs() -> bool {
    Command::new("mkfs.xfs")
        .arg("-V")
        .output()
        .is_ok_and(|o| o.status.success())
}

fn xfs_db(image: &str, args: &[String]) -> String {
    let out = Command::new("xfs_db")
        .args(args)
        .arg(image)
        .output()
        .expect("xfs_db");
    assert!(
        out.status.success(),
        "xfs_db {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `(namespace flag, on-disk name, value length)`, and what each file's
/// attribute fork should become.
fn plan() -> Vec<(
    &'static str,
    &'static str,
    Vec<(&'static str, String, usize)>,
)> {
    let many = |n: usize, len: usize| {
        (0..n)
            .map(|i| {
                let ns = ["-u", "-r", "-s"][i % 3];
                (ns, format!("attribute_number_{i:03}"), len)
            })
            .collect::<Vec<_>>()
    };
    vec![
        (
            "short",
            "1 (local)",
            vec![
                ("-u", "a".into(), 5),
                ("-r", "t".into(), 1),
                ("-s", "sec".into(), 12),
            ],
        ),
        ("leaf", "0x3bee", many(20, 40)),
        ("node", "0x3ebe", many(400, 60)),
        (
            "remote",
            "0x3bee",
            vec![
                ("-u", "big".into(), 30_000),
                ("-u", "small".into(), 3),
                ("-s", "huge".into(), 65_536),
            ],
        ),
    ]
}

#[test]
fn attributes_read_back_as_xfs_db_wrote_them() {
    if !xfsprogs() {
        eprintln!("skip: xfsprogs not installed");
        return;
    }
    let root = std::env::temp_dir().join(format!("fs_xfs_xattr_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let empty = root.join("empty");
    std::fs::write(&empty, b"").unwrap();
    let plan = plan();
    let mut proto = String::from("/dev/null\n0 0\nd--755 0 0\n");
    for (file, _, _) in &plan {
        proto.push_str(&format!("{file} ---644 0 0 {}\n", empty.display()));
    }
    proto.push_str("plain ---644 0 0 ");
    proto.push_str(&format!("{}\n$\n", empty.display()));
    std::fs::write(root.join("proto"), proto).unwrap();
    let image = root.join("fs.img");
    std::fs::File::create(&image)
        .and_then(|f| f.set_len(300 * 1024 * 1024))
        .unwrap();
    let out = Command::new("mkfs.xfs")
        .args(["-q", "-f", "-p"])
        .arg(root.join("proto"))
        .arg(&image)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let image = image.to_str().unwrap().to_string();

    let inos: BTreeMap<&str, u64> = {
        let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
        plan.iter()
            .map(|(file, _, _)| (*file, fs.open(&format!("/{file}")).unwrap().inode().ino))
            .collect()
    };
    for (file, _, attrs) in &plan {
        let mut args = vec![
            "-x".to_string(),
            "-c".into(),
            format!("inode {}", inos[file]),
        ];
        for (ns, name, len) in attrs {
            args.push("-c".into());
            args.push(format!("attr_set {ns} -v {len} {name}"));
        }
        xfs_db(&image, &args);
    }
    let repair = Command::new("xfs_repair")
        .args(["-n", &image])
        .output()
        .unwrap();
    assert!(
        repair.status.success(),
        "{}",
        String::from_utf8_lossy(&repair.stdout)
    );

    let fs = Filesystem::mount(Arc::new(FileDevice::open(&image).unwrap())).unwrap();
    for (file, shape, attrs) in &plan {
        // The shape, as xfs_db reports it.
        let report = if shape.starts_with("0x") {
            xfs_db(
                &image,
                &[
                    "-r".into(),
                    "-c".into(),
                    format!("inode {}", inos[file]),
                    "-c".into(),
                    "ablock 0".into(),
                    "-c".into(),
                    "p hdr.info.hdr.magic".into(),
                ],
            )
        } else {
            xfs_db(
                &image,
                &[
                    "-r".into(),
                    "-c".into(),
                    format!("inode {}", inos[file]),
                    "-c".into(),
                    "p core.aformat".into(),
                ],
            )
        };
        assert!(report.contains(shape), "[{file}] fixture shape: {report}");

        let handle = fs.open(&format!("/{file}")).unwrap();
        let got: BTreeMap<Vec<u8>, Vec<u8>> = fs
            .list_xattrs(handle.inode(), handle.raw())
            .unwrap_or_else(|e| panic!("[{file}] {e:?}"))
            .into_iter()
            .map(|a| (a.name, a.value))
            .collect();
        let want: BTreeMap<Vec<u8>, Vec<u8>> = attrs
            .iter()
            .map(|(ns, name, len)| {
                let prefix = match *ns {
                    "-r" => "trusted.",
                    "-s" => "security.",
                    _ => "user.",
                };
                (format!("{prefix}{name}").into_bytes(), vec![b'v'; *len])
            })
            .collect();
        assert_eq!(got.len(), want.len(), "[{file}] attribute count");
        for (name, value) in &want {
            let found = got
                .get(name)
                .unwrap_or_else(|| panic!("[{file}] {} is missing", String::from_utf8_lossy(name)));
            assert!(
                found == value,
                "[{file}] {} has {} bytes, want {}",
                String::from_utf8_lossy(name),
                found.len(),
                value.len()
            );
        }
        let (name, value) = want.iter().next().unwrap();
        assert_eq!(
            fs.get_xattr(handle.inode(), handle.raw(), name)
                .unwrap()
                .as_ref(),
            Some(value),
            "[{file}] get_xattr"
        );
        assert_eq!(
            fs.get_xattr(handle.inode(), handle.raw(), b"user.not-set")
                .unwrap(),
            None
        );
    }
    let plain = fs.open("/plain").unwrap();
    assert!(fs
        .list_xattrs(plain.inode(), plain.raw())
        .unwrap()
        .is_empty());

    let _ = std::fs::remove_dir_all(&root);
}
