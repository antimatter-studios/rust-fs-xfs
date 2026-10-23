//! `fs.xfs mv`, judged by THE KERNEL and by xfsprogs.
//!
//! Linux makes and fills a v5 volume in the harness guest, then copies it
//! to a twin and makes a chain of renames on the twin with `rename(2)`
//! itself: a rename in place, moves into and out of a directory past short
//! form, one name of a hard-linked file, a directory to another directory,
//! a name that is not UTF-8, a symlink (never what it points at), a file
//! replacing a file and a directory replacing an empty one. `fs.xfs mv`
//! makes the same chain on the original. The kernel's mount of each must
//! then show the same tree, name by name (type, links, inode number,
//! content), every file must still hold the bytes it held, and
//! `xfs_repair -n` must find nothing wrong with ours. The moves release
//! the volume clean, so `xfs_repair` grades it as it is, with no replay.
//!
//! The second test puts every refusal to the kernel first: `rename(2)`
//! on the kernel's mount, and `fs.xfs mv` must refuse the same pair of
//! paths for the same reason, leaving the image byte-identical.

mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, repair, scratch};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

const SUITE: &str = "cli_mv_kernel";

/// A shell function: `list DIR TAG` prints every path under DIR, the root
/// included, one line each as `TAG path|type|links|inode|content`, the
/// content being a regular file's SHA-256 and a symlink's target.
const LIST: &str = r#"
list() {
    ( cd "$1" && find . | LC_ALL=C sort | while read -r p; do
        case "$(stat -c %F -- "$p")" in
            regular*) content="$(sha256sum <"$p" | cut -d' ' -f1)" ;;
            symbolic*) content="$(readlink -- "$p")" ;;
            *) content=- ;;
        esac
        q="${p#.}"
        printf '%s %s|%s|%s\n' "$2" "${q:-/}" "$(stat -c '%F|%h|%i' -- "$p")" "$content"
    done )
}
"#;

/// A shell function: `rn FROM TO` is `rename(2)`, with nothing in between
/// to check or retry it, as `mv` would. A refusal prints the kernel's
/// reason, as strerror words it, and fails.
const RENAME: &str = r#"
rn() {
    python3 -c '
import os, sys
try:
    os.rename(sys.argv[1], sys.argv[2])
except OSError as e:
    print(os.strerror(e.errno))
    sys.exit(1)
' "$1" "$2"
}
"#;

/// A path's line from [`LIST`]: type, links, inode, content.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    kind: String,
    links: u64,
    inode: String,
    content: String,
}

/// The lines of `out` tagged `tag`, by path.
fn listing(out: &str, tag: &str) -> BTreeMap<String, Seen> {
    let prefix = format!("{tag} ");
    let seen: BTreeMap<String, Seen> = out
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix))
        .map(|line| {
            let f: Vec<&str> = line.split('|').collect();
            assert_eq!(f.len(), 5, "a guest listing line: {line}");
            let seen = Seen {
                kind: f[1].to_string(),
                links: f[2].parse().unwrap(),
                inode: f[3].to_string(),
                content: f[4].to_string(),
            };
            (f[0].to_string(), seen)
        })
        .collect();
    assert!(
        seen.contains_key("/"),
        "the guest printed no {tag} listing:\n{out}"
    );
    seen
}

/// `bytes` as one bash word, `$'...'`, every byte outside printable ASCII
/// (and each quote and backslash) escaped, so a name that is not UTF-8
/// reaches the guest as its bytes.
fn bash_word(bytes: &[u8]) -> String {
    let mut word = String::from("$'");
    for &b in bytes {
        match b {
            b'\'' | b'\\' => word.push_str(&format!("\\{}", b as char)),
            0x20..=0x7e => word.push(b as char),
            _ => word.push_str(&format!("\\x{b:02x}")),
        }
    }
    word.push('\'');
    word
}

/// The chain of moves both sides make, in order: SOURCE, then PATH.
const MOVES: [(&[u8], &[u8]); 10] = [
    (b"/file1", b"/renamed"),
    (b"/small", b"/full/small"),
    (b"/many/e50", b"/e50"),
    (b"/twin", b"/many/twin"),
    (b"/full", b"/dest/full"),
    (b"/raw-\xff", b"/raw-moved-\xff"),
    (b"/dirlink", b"/dl"),
    (b"/renamed", b"/victim"),
    (b"/many", b"/emptyvictim"),
    (b"/dest/full/sub", b"/sub"),
];

#[test]
fn mv_leaves_the_tree_the_kernel_leaves_on_a_twin() {
    let image = scratch::Volume::empty(SUITE, &format!("{}-twin.img", std::process::id()), 0);
    let img = image.path().to_str().unwrap().to_string();
    let renames: String = MOVES
        .iter()
        .map(|(from, to)| {
            let (from, to) = (bash_word(&from[1..]), bash_word(&to[1..]));
            format!("rn {from} {to} || echo RENAME_FAILED {from} {to}\n")
        })
        .collect();
    let made = kernel_run(&format!(
        r#"
        {LIST}
        {RENAME}
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -m crc=1 "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        ( cd "$work/m"
          mkdir -p full/sub dest many emptyvictim
          head -c 9000 /dev/urandom > file1
          head -c 7000 /dev/urandom > kept
          ln kept twin
          ln -s full dirlink
          printf x > small
          head -c 3000 /dev/urandom > victim
          head -c 5000 /dev/urandom > full/inner
          for i in $(seq 1 100); do printf '%s' "$i" > "many/e$i"; done
          printf raw > $'raw-\xff'
        )
        sync
        umount "$work/m" || echo UMOUNT_FAILED
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        list "$work/m" BEFORE
        umount "$work/m" || echo UMOUNT_FAILED
        cp --sparse=always "$work/image" "$work/twin"
        mount -o loop,nouuid "$work/twin" "$work/m"
        ( cd "$work/m"
          {renames}
        )
        umount "$work/m" || echo UMOUNT_FAILED
        mount -o loop,ro,nouuid "$work/twin" "$work/m"
        list "$work/m" TWIN
        umount "$work/m" || echo UMOUNT_FAILED
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(&img),
    ));
    assert!(
        !made.contains("RENAME_FAILED") && !made.contains("UMOUNT_FAILED"),
        "the kernel made every rename on the twin:\n{made}"
    );
    let before = listing(&made, "BEFORE");
    let twin = listing(&made, "TWIN");
    assert_eq!(
        before["/kept"].links, 2,
        "the kernel's hard link: {before:?}"
    );
    assert_eq!(before["/twin"], before["/kept"], "{before:?}");

    for (from, to) in MOVES {
        let shown = String::from_utf8_lossy(from).into_owned();
        assert!(
            !twin.contains_key(&shown),
            "the kernel moved {shown} away on the twin: {twin:?}"
        );
        let report = stdout(&ok(tool("fs.xfs")
            .arg(&img)
            .arg("mv")
            .arg(OsStr::from_bytes(from))
            .arg(OsStr::from_bytes(to))));
        assert_eq!(json_field(&report, "moved"), "true", "{report}");
        if std::str::from_utf8(to).is_err() {
            let hex: String = to.iter().map(|b| format!("{b:02x}")).collect();
            assert_eq!(json_field(&report, "path_hex"), hex, "{report}");
        } else {
            assert_eq!(
                json_field(&report, "path"),
                String::from_utf8_lossy(to),
                "{report}"
            );
        }
    }
    let dirty = stdout(&ok(tool("fs.xfs").args([&img, "get", "dirty", "--text"])));
    assert_eq!(dirty.trim(), "false", "the moves were not released clean");

    let after = kernel_run(&format!(
        r#"
        {LIST}
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        cp --sparse=always {image} "$work/image"
        {repair}
        mkdir "$work/m"
        mount -o loop,ro,nouuid "$work/image" "$work/m"
        list "$work/m" AFTER
        umount "$work/m" || echo UMOUNT_FAILED
        echo DONE
        "#,
        image = guest_quote(&img),
        repair = repair::script("\"$work/image\""),
    ));
    repair::assert_agreed(&after, "the volume fs.xfs mv left");
    let ours = listing(&after, "AFTER");
    assert_eq!(
        ours, twin,
        "the kernel's view of the volume fs.xfs moved names in (left) is not its view \
         of the twin it renamed the same names in (right)"
    );

    // Every file kept its bytes under its new name: by inode, what the
    // kernel read before any move is what it reads after, and only the
    // replaced /victim is gone.
    let files = |seen: &BTreeMap<String, Seen>| -> BTreeMap<String, String> {
        seen.values()
            .filter(|s| s.kind.starts_with("regular"))
            .map(|s| (s.inode.clone(), s.content.clone()))
            .collect()
    };
    let mut kept = files(&before);
    assert!(
        kept.remove(&before["/victim"].inode).is_some(),
        "{before:?}"
    );
    assert_eq!(
        files(&ours),
        kept,
        "a file's bytes changed with its name, or the replaced file survived"
    );
    assert_eq!(ours["/victim"].inode, before["/file1"].inode, "{ours:?}");
    assert_eq!(ours["/emptyvictim/twin"].links, 2, "{ours:?}");
    assert_eq!(ours["/dl"].content, "full", "{ours:?}");
    assert_eq!(ours["/emptyvictim/e1"], before["/many/e1"], "{ours:?}");
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// `rename(2)` returned.
fn ours(reason: &str) -> &'static str {
    match reason {
        "No such file or directory" => "no such file or directory",
        "Is a directory" => "is a directory",
        "Not a directory" => "not a directory",
        // XFS answers a directory replacing one with entries EEXIST, which
        // POSIX allows beside ENOTEMPTY; the VFS says ENOTEMPTY first when
        // the target is the source's own ancestor.
        "Directory not empty" | "File exists" => "directory is not empty",
        "Invalid argument" => "invalid argument",
        "Device or resource busy" => "cannot be moved or replaced",
        "File name too long" => "file name too long",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn mv_refuses_what_the_kernel_refuses() {
    let image = scratch::Volume::empty(SUITE, &format!("{}-refusals.img", std::process::id()), 0);
    let img = image.path().to_str().unwrap().to_string();
    let long = "n".repeat(256);
    // Each case: SOURCE and PATH as fs.xfs is given them; the guest makes
    // the same `rename(2)` relative to its mount.
    let cases: Vec<(String, String)> = [
        ("/missing", "/x"),
        ("/missing/x", "/y"),
        ("/file", "/missing/x"),
        ("/file", "/full/inner/x"),
        ("/file/", "/x"),
        ("/file", "/x/"),
        ("/full", "/file"),
        ("/full", "/dirlink"),
        ("/file", "/full"),
        ("/empty", "/full"),
        ("/full/sub", "/full"),
        ("/full", "/full/x"),
        ("/full", "/full/sub/x"),
        ("/full/.", "/x"),
        ("/full/..", "/x"),
        ("/file", "/full/."),
        ("/file", "/full/.."),
        ("/file", &format!("/{long}")),
    ]
    .iter()
    .map(|(s, p)| (s.to_string(), p.to_string()))
    .collect();
    let mut tries = String::new();
    for (source, path) in &cases {
        tries.push_str(&format!(
            "refuse '{source} {path}' '{}' '{}'\n",
            &source[1..],
            &path[1..]
        ));
    }
    let out = kernel_run(&format!(
        r#"
        {RENAME}
        work=$(mktemp -d)
        trap 'cd /; mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -m crc=1 "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        cd "$work/m"
        mkdir -p full/sub empty
        printf x > full/inner
        printf y > file
        ln -s full dirlink
        sync
        refuse() {{
            label="$1"
            if out="$(LC_ALL=C rn "$2" "$3" 2>&1)"; then
                printf 'REFUSAL %s|NOT REFUSED\n' "$label"
            else
                printf 'REFUSAL %s|%s\n' "$label" "$out"
            fi
        }}
        {tries}
        cd /
        umount "$work/m" || echo UMOUNT_FAILED
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        image = guest_quote(&img),
    ));
    let before = std::fs::read(&img).expect("read the image");

    let mut tried = Vec::new();
    for line in out.lines().filter_map(|l| l.strip_prefix("REFUSAL ")) {
        let (label, reason) = line
            .split_once('|')
            .unwrap_or_else(|| panic!("a guest refusal line: {line}"));
        let want = ours(reason);
        let (source, path) = label.split_once(' ').unwrap();
        let got = tool("fs.xfs")
            .args([&img, "mv", source, path])
            .output()
            .expect("spawn fs.xfs");
        let err = stderr(&got);
        let what = format!("mv {label}: the kernel said {reason:?}; fs.xfs said {err}");
        let code = refusal_code(reason);
        assert_eq!(got.status.code(), Some(code), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains(&format!("\"code\": {code}")), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == before,
            "mv {label} was refused and still changed the image"
        );
        tried.push(label.to_string());
    }
    let labels: Vec<String> = cases.iter().map(|(s, p)| format!("{s} {p}")).collect();
    assert_eq!(tried, labels, "the guest answered every case:\n{out}");
}
