//! `fs.xfs export` and `import` under `--preserve=xattrs`, judged by THE
//! KERNEL and by the host's own attribute calls.
//!
//! - a tree the kernel made, with local, empty, binary and remote-valued
//!   attributes on a file and a directory, exports to the host with the
//!   names and values `getfattr` reports for it in the guest, read back on
//!   the host with `os.listxattr`/`os.getxattr`;
//! - a host tree with the same shapes of attribute imports into a copy of
//!   the kernel-made CLI image, and the guest's `getfattr` lists them with
//!   the host's values once Linux has mounted it; `xfs_repair -n` grades
//!   the volume clean.
//!
//! Only the `user.` namespace is used: a host that is not root cannot set
//! `trusted.` attributes, and this suite runs both as the runner's user and
//! as root in the guest. A namespace the other side cannot hold is refused
//! per item; the shell test `tests/cli/test-xattrs.sh` checks that.

mod cli_support;
mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use cli_support::*;
use common::{fixture, guest_quote, kernel_run, repair, scratch};

const SUITE: &str = "cli_xattr_transfer_kernel";

type Attrs = BTreeMap<Vec<u8>, Vec<u8>>;

fn unhex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd hex: {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The attributes every case uses: in the shortform fork, empty, with
/// bytes no text holds, and one too large to stay local.
fn shapes(tag: &str) -> Attrs {
    BTreeMap::from([
        (b"user.text".to_vec(), format!("{tag} value").into_bytes()),
        (b"user.empty".to_vec(), Vec::new()),
        (b"user.binary".to_vec(), b"\x00\xff\n\x80".to_vec()),
        (
            b"user.remote".to_vec(),
            (0..3000u32).map(|i| (i % 251) as u8).collect(),
        ),
    ])
}

/// `ATTR|<label>|<name hex>|<value hex>` lines, as both sides print them.
fn parse(report: &str, label: &str) -> Attrs {
    report
        .lines()
        .filter_map(|l| l.strip_prefix(&format!("ATTR|{label}|")))
        .map(|l| {
            let (name, value) = l.split_once('|').unwrap();
            (unhex(name), unhex(value))
        })
        .collect()
}

/// Run the host's own attribute calls, which is Python's `os` module over
/// `listxattr(2)` and `getxattr(2)`, never this crate.
fn host_python(script: &str, args: &[&Path]) -> String {
    let out = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("python3 would not run (chore tools provides it): {e}"));
    assert!(
        out.status.success(),
        "the host's attribute calls failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

const HOST_DUMP: &str = r#"
import os, sys
for path, label in zip(sys.argv[1::2], sys.argv[2::2]):
    for name in os.listxattr(path, follow_symlinks=False):
        value = os.getxattr(path, name, follow_symlinks=False)
        print('ATTR|' + label + '|' + name.encode().hex() + '|' + value.hex())
"#;

fn host_attrs(path: &Path, label: &str) -> Attrs {
    let out = host_python(HOST_DUMP, &[path, Path::new(label)]);
    // Only what this suite set: a host may add its own (a security label).
    parse(&out, label)
        .into_iter()
        .filter(|(n, _)| n.starts_with(b"user."))
        .collect()
}

fn host_set(path: &Path, attrs: &Attrs) {
    let mut script = String::from("import os, sys\np = sys.argv[1]\n");
    for (name, value) in attrs {
        script.push_str(&format!(
            "os.setxattr(p, bytes.fromhex('{}'), bytes.fromhex('{}'), follow_symlinks=False)\n",
            hex(name),
            hex(value)
        ));
    }
    host_python(&script, &[path]);
}

/// A host directory for one case, on the host's own disk. Not the shared
/// folder, and not `TMPDIR`: when the suite runs in the guest that is
/// `/repo/tmp`, on the shared folder, which refuses an empty attribute
/// value with ENODATA (run 40500633367). `/var/tmp` is the root file
/// system on the runner and in the guest alike.
fn host_dir(tag: &str) -> PathBuf {
    let dir = Path::new("/var/tmp").join(format!("{SUITE}-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The guest's `getfattr` dump of each of `paths` under the mount, as
/// `ATTR|<path>|...` lines, after Linux has mounted `image`; then the
/// unmount, and `xfs_repair -n`.
fn kernel_dump(image: &str, paths: &[&str]) -> String {
    let dumps: String = paths
        .iter()
        .map(|p| {
            format!(
                "getfattr --absolute-names -d -m - -e hex \"$work/m{p}\" | python3 -c \"$parse\" '{p}'\n"
            )
        })
        .collect();
    let out = kernel_run(&format!(
        r##"
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        mkdir "$work/m"
        parse='
import sys
label = sys.argv[1]
for line in sys.stdin.buffer.read().splitlines():
    if b"=" not in line or line.startswith(b"#"):
        continue
    name, value = line.split(b"=", 1)
    value = b"" if value == b"\"\"" else bytes.fromhex(value[2:].decode())
    print("ATTR|" + label + "|" + name.hex() + "|" + value.hex())
'
        if mount -o loop,nouuid "$work/image" "$work/m"; then
            echo MOUNT_OK
            {dumps}
            umount "$work/m" || (echo UMOUNT_FAILED; dmesg | tail -15)
        else
            echo MOUNT_REFUSED
        fi
        {repair}
        echo DONE
        "##,
        image = guest_quote(image),
        repair = repair::script("\"$work/image\""),
    ));
    assert!(
        out.contains("MOUNT_OK"),
        "the kernel would not mount:\n{out}"
    );
    assert!(!out.contains("UMOUNT_FAILED"), "{out}");
    out
}

#[test]
fn an_export_carries_the_attributes_the_kernel_set() {
    let image = scratch::Volume::empty(SUITE, "export.img", 320 << 20);
    let img = image.path().to_str().unwrap();
    let (file, dir) = (shapes("file"), shapes("dir"));
    let sets = |path: &str, attrs: &Attrs| -> String {
        attrs
            .iter()
            .map(|(n, v)| {
                format!(
                    "os.setxattr(root + '{path}', bytes.fromhex('{}'), bytes.fromhex('{}'))\n",
                    hex(n),
                    hex(v)
                )
            })
            .collect()
    };
    let report = kernel_run(&format!(
        r##"
        set -eu
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        mkdir -p "$work/m/t/d"
        echo data > "$work/m/t/d/f"
        python3 - "$work/m" <<'PY'
import os, sys
root = sys.argv[1]
{file_sets}{dir_sets}
PY
        for p in /t/d/f /t/d; do
            getfattr --absolute-names -d -m - -e hex "$work/m$p" | python3 -c '
import sys
label = sys.argv[1]
for line in sys.stdin.buffer.read().splitlines():
    if b"=" not in line or line.startswith(b"#"):
        continue
    name, value = line.split(b"=", 1)
    value = b"" if value == b"\"\"" else bytes.fromhex(value[2:].decode())
    print("ATTR|" + label + "|" + name.hex() + "|" + value.hex())
' "$p"
        done
        umount "$work/m" || echo UMOUNT_FAILED
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "##,
        file_sets = sets("/t/d/f", &file),
        dir_sets = sets("/t/d", &dir),
        image = guest_quote(img),
    ));
    assert!(
        report.contains("DONE") && !report.contains("UMOUNT_FAILED"),
        "{report}"
    );
    let kernel_file = parse(&report, "/t/d/f");
    let kernel_dir = parse(&report, "/t/d");
    assert_eq!(kernel_file, file, "the kernel did not set what was asked");
    assert_eq!(kernel_dir, dir, "the kernel did not set what was asked");

    let host = host_dir("export");
    let dest = host.join("out");
    let out = ok(tool("fs.xfs").args([
        img,
        "export",
        "--recursive",
        "--preserve=xattrs",
        "/t",
        dest.to_str().unwrap(),
    ]));
    let report = stdout(&out);
    assert_eq!(json_field(&report, "completed"), "3", "{report}");
    assert_eq!(json_field(&report, "refused"), "0", "{report}");
    assert_eq!(host_attrs(&dest.join("d/f"), "f"), kernel_file);
    assert_eq!(host_attrs(&dest.join("d"), "d"), kernel_dir);
    assert_eq!(std::fs::read(dest.join("d/f")).unwrap(), b"data\n");
    let _ = std::fs::remove_dir_all(&host);
}

#[test]
fn an_import_carries_host_attributes_the_kernel_lists() {
    let volume = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-import.img", std::process::id()),
    );
    let img = volume.path().to_str().unwrap();
    let host = host_dir("import");
    let tree = host.join("in");
    std::fs::create_dir_all(tree.join("d")).unwrap();
    std::fs::write(tree.join("d/f"), b"data\n").unwrap();
    let (file, dir) = (shapes("file"), shapes("dir"));
    host_set(&tree.join("d/f"), &file);
    host_set(&tree.join("d"), &dir);

    let out = ok(tool("fs.xfs").args([
        img,
        "import",
        "--recursive",
        "--preserve=xattrs",
        tree.to_str().unwrap(),
        "/in",
    ]));
    let report = stdout(&out);
    assert_eq!(json_field(&report, "completed"), "3", "{report}");
    assert_eq!(json_field(&report, "refused"), "0", "{report}");

    let out = kernel_dump(img, &["/in/d/f", "/in/d"]);
    let user = |attrs: Attrs| -> Attrs {
        attrs
            .into_iter()
            .filter(|(n, _)| n.starts_with(b"user."))
            .collect()
    };
    assert_eq!(user(parse(&out, "/in/d/f")), file, "{out}");
    assert_eq!(user(parse(&out, "/in/d")), dir, "{out}");
    repair::assert_agreed(&out, "the volume an attribute import left");
    let _ = std::fs::remove_dir_all(&host);
}
