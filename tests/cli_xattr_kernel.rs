//! CLI attributes against Linux setxattr/getfattr in the harness guest.
//! xfs_db independently identifies local, leaf, node and remote forks.
mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, oracle, scratch};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

fn unhex(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "odd hex: {hex}");
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

#[test]
fn every_fork_lists_and_reads_the_kernels_names_and_values() {
    let image = scratch::Volume::empty("cli-xattr-kernel", "attrs.img", 320 << 20);
    let img = image.path().to_str().unwrap();
    let report = kernel_run(&format!(
        r#"
        set -eu
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -i size=512 "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        python3 - "$work/m" <<'PY'
import os, subprocess, sys
root = sys.argv[1]
for shape, count in [('local', 0), ('leaf', 20), ('node', 400), ('remote', 0)]:
    path = root + '/' + shape
    open(path, 'wb').close()
    attrs = {{b'user.binary': b'\x00\xff\n\x80', b'user.empty': b'',
             b'trusted.note': b'root', b'security.test': b'label'}}
    for i in range(count):
        attrs[('user.attribute_number_%03d' % i).encode()] = bytes(range(60))
    if shape == 'remote':
        attrs[b'user.large'] = bytes(range(256)) * 256
    for name, value in attrs.items():
        os.setxattr(path, name, value)
    dump = subprocess.check_output(['getfattr', '--absolute-names', '-d', '-m', '-', '-e', 'hex', path])
    seen = {{}}
    for line in dump.splitlines():
        if b'=' not in line or line.startswith(b'#'):
            continue
        name, value = line.split(b'=', 1)
        assert value == b'""' or value.startswith(b'0x'), line
        seen[name] = b'' if value == b'""' else bytes.fromhex(value[2:].decode())
        print('ATTR|' + shape + '|' + name.hex() + '|' + seen[name].hex())
    assert seen == attrs, (shape, seen.keys(), attrs.keys())
    if shape == 'local':
        # A byte name cannot be represented as UTF-8 in JSON or argv text.
        name = b'user.raw-\xff'
        os.setxattr(path, name, b'raw\x00value')
        print('ATTR|local|' + name.hex() + '|' + os.getxattr(path, name).hex())
    print('INO|' + shape + '|' + str(os.stat(path).st_ino))
PY
        umount "$work/m"
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(img),
    ));
    let before = sha256_hex(&std::fs::read(img).unwrap());
    for (shape, magic) in [
        ("local", "1 (local)"),
        ("leaf", "0x3bee"),
        ("node", "0x3ebe"),
        ("remote", "0x3bee"),
    ] {
        let ino = report
            .lines()
            .find_map(|l| l.strip_prefix(&format!("INO|{shape}|")))
            .expect("the kernel printed the inode");
        let db = oracle("xfs_db").args(["-r", "-c", &format!("inode {ino}")]);
        let db = if shape == "local" {
            db.args(["-c", "p core.aformat"])
        } else {
            db.args(["-c", "ablock 0", "-c", "p hdr.info.hdr.magic"])
        };
        let form = db.arg(img).output();
        assert!(form.ok(), "{}{}", form.stdout, form.stderr);
        assert!(form.stdout.contains(magic), "{shape}: {}", form.stdout);
        let expected: BTreeMap<Vec<u8>, Vec<u8>> = report
            .lines()
            .filter_map(|l| l.strip_prefix(&format!("ATTR|{shape}|")))
            .map(|l| {
                let (name, value) = l.split_once('|').unwrap();
                (unhex(name), unhex(value))
            })
            .collect();
        assert_eq!(
            expected.len(),
            match shape {
                "local" | "remote" => 5,
                "leaf" => 24,
                "node" => 404,
                _ => unreachable!(),
            }
        );
        let path = format!("/{shape}");
        let listing = stdout(&ok(tool("fs.xfs").args([img, "xattr", &path])));
        let mut actual = BTreeMap::new();
        for row in listing.split("\n  {").skip(1) {
            let name = if row.contains("\"name_hex\"") {
                assert!(json_field(row, "name").contains('\u{fffd}'));
                unhex(&json_field(row, "name_hex"))
            } else {
                json_field(row, "name").into_bytes()
            };
            assert!(actual
                .insert(name, json_field(row, "size").parse::<usize>().unwrap())
                .is_none());
        }
        assert_eq!(
            actual,
            expected.iter().map(|(n, v)| (n.clone(), v.len())).collect()
        );
        let text = stdout(&ok(tool("fs.xfs").args([img, "xattr", &path, "--text"])));
        assert!(text.lines().any(|l| l == "user.binary\t4"), "{text}");
        for (name, value) in &expected {
            let out = ok(tool("fs.xfs")
                .args([img, "xattr", &path])
                .arg(OsString::from_vec(name.clone())));
            assert!(out.stderr.is_empty(), "{}", stderr(&out));
            assert_eq!(&out.stdout, value, "{shape}: {name:?}");
        }
        let dest = image.path().with_extension("value");
        std::fs::write(&dest, b"old output").unwrap();
        let out = ok(tool("fs.xfs")
            .args([img, "xattr", &path, "user.binary", "-o"])
            .arg(&dest));
        assert!(out.stdout.is_empty());
        assert_eq!(
            std::fs::read(&dest).unwrap(),
            expected[b"user.binary".as_slice()]
        );
        assert!(!dest.with_extension("value.partial").exists());
    }
    assert_eq!(sha256_hex(&std::fs::read(img).unwrap()), before);
}
