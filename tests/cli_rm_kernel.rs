//! `fs.xfs rm` and `rmdir`, judged by THE KERNEL and by xfsprogs.
//!
//! Linux makes and fills a v5 volume in the harness guest, then copies it
//! to a twin and removes names from the twin with its own `rm` and `rmdir`:
//! a file, one name of a hard-linked file, a symlink to a directory, a name
//! in a directory past short form, a name that is not UTF-8, an empty
//! directory and an empty subdirectory. `fs.xfs` removes the same names
//! from the original. The kernel's mount of each must then show the same
//! tree, name by name (type, links, inode number, content), and
//! `xfs_repair -n` must find nothing wrong with ours. The removal releases
//! the volume clean, so `xfs_repair` grades it as it is, with no replay.
//!
//! The second test puts every refusal to the kernel first: the guest's
//! `unlink` and `rmdir` commands, which make the system calls of those
//! names, try each one on the kernel's mount, and `fs.xfs` must refuse
//! the same paths for the same reason, leaving the image byte-identical.

mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, repair, scratch};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

const SUITE: &str = "cli_rm_kernel";

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

/// The name that is not UTF-8, as the listing shows it and as bytes.
const RAW: &[u8] = b"/raw-\xff";

#[test]
fn rm_and_rmdir_leave_the_tree_the_kernel_leaves_on_a_twin() {
    let image = scratch::Volume::empty(SUITE, &format!("{}-twin.img", std::process::id()), 0);
    let img = image.path().to_str().unwrap().to_string();
    let made = kernel_run(&format!(
        r#"
        {LIST}
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -m crc=1 "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        ( cd "$work/m"
          mkdir -p full/sub empty many
          head -c 9000 /dev/urandom > gone
          head -c 7000 /dev/urandom > kept
          ln kept twin
          ln -s full dirlink
          printf x > small
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
          rm gone twin dirlink many/e50 $'raw-\xff'
          rmdir empty full/sub
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
    let before = listing(&made, "BEFORE");
    let twin = listing(&made, "TWIN");
    assert_eq!(
        before["/kept"].links, 2,
        "the kernel's hard link: {before:?}"
    );
    assert_eq!(before["/twin"], before["/kept"], "{before:?}");

    let removals: [(&str, &[u8]); 7] = [
        ("rm", b"/gone"),
        ("rm", b"/twin"),
        ("rm", b"/dirlink"),
        ("rm", b"/many/e50"),
        ("rm", RAW),
        ("rmdir", b"/empty"),
        ("rmdir", b"/full/sub"),
    ];
    for (verb, path) in removals {
        let shown = String::from_utf8_lossy(path).into_owned();
        assert!(
            before.contains_key(&shown) && !twin.contains_key(&shown),
            "the kernel removed {shown} from the twin"
        );
        let report = stdout(&ok(tool("fs.xfs")
            .arg(&img)
            .arg(verb)
            .arg(OsStr::from_bytes(path))));
        assert_eq!(json_field(&report, "removed"), "true", "{report}");
        if path == RAW {
            assert_eq!(json_field(&report, "path_hex"), "2f7261772dff", "{report}");
        } else {
            assert_eq!(json_field(&report, "path"), shown, "{report}");
        }
    }
    let dirty = stdout(&ok(tool("fs.xfs").args([&img, "get", "dirty", "--text"])));
    assert_eq!(
        dirty.trim(),
        "false",
        "the removals were not released clean"
    );

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
    repair::assert_agreed(&after, "the volume fs.xfs rm and rmdir left");
    let ours = listing(&after, "AFTER");
    assert_eq!(
        ours, twin,
        "the kernel's view of the volume fs.xfs removed from (left) is not its view of \
         the twin it removed the same names from (right)"
    );
    assert_eq!(ours["/kept"].links, 1, "{ours:?}");
    assert_eq!(ours["/kept"].content, before["/kept"].content, "{ours:?}");
}

/// Our message for the kernel's reason: the text strerror gives the errno
/// the guest's command reported.
fn ours(reason: &str) -> &'static str {
    match reason {
        "No such file or directory" => "no such file or directory",
        "Is a directory" => "is a directory",
        "Not a directory" => "not a directory",
        "Directory not empty" => "directory is not empty",
        "Invalid argument" => "invalid argument",
        "Device or resource busy" => "the root directory cannot be removed",
        "NOT REFUSED" => panic!("the kernel did not refuse it"),
        other => panic!("no message is mapped for the kernel's {other:?}"),
    }
}

#[test]
fn rm_and_rmdir_refuse_what_the_kernel_refuses() {
    let image = scratch::Volume::empty(SUITE, &format!("{}-refusals.img", std::process::id()), 0);
    let img = image.path().to_str().unwrap().to_string();
    // Each case: our verb and path, then the guest command that makes the
    // same system call on the kernel's mount. `rmdir "$work/m"` is the root.
    let cases = [
        ("rm /missing", "unlink missing"),
        ("rmdir /missing", "rmdir missing"),
        ("rm /missing/x", "unlink missing/x"),
        ("rm /full", "unlink full"),
        ("rm /full/.", "unlink full/."),
        ("rm /full/..", "unlink full/.."),
        ("rm /file/", "unlink file/"),
        ("rm /dirlink/", "unlink dirlink/"),
        ("rm /file/x", "unlink file/x"),
        ("rmdir /file", "rmdir file"),
        ("rmdir /dirlink", "rmdir dirlink"),
        ("rmdir /full", "rmdir full"),
        ("rmdir /full/..", "rmdir full/.."),
        ("rmdir /full/.", "rmdir full/."),
        ("rmdir /", "rmdir \"$work/m\""),
    ];
    let mut tries = String::new();
    for (label, command) in cases {
        tries.push_str(&format!("refuse '{label}' {command}\n"));
    }
    let out = kernel_run(&format!(
        r#"
        work=$(mktemp -d)
        trap 'cd /; mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f -m crc=1 "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        cd "$work/m"
        mkdir full
        printf x > full/inner
        printf y > file
        ln -s full dirlink
        sync
        refuse() {{
            label="$1"
            shift
            if out="$(LC_ALL=C "$@" 2>&1)"; then
                printf 'REFUSAL %s|NOT REFUSED\n' "$label"
            else
                printf 'REFUSAL %s|%s\n' "$label" "${{out##*: }}"
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
        let (verb, path) = label.split_once(' ').unwrap();
        let got = tool("fs.xfs")
            .args([&img, verb, path])
            .output()
            .expect("spawn fs.xfs");
        let err = stderr(&got);
        let what = format!("{label}: the kernel said {reason:?}; fs.xfs said {err}");
        let code = refusal_code(reason);
        assert_eq!(got.status.code(), Some(code), "{what}");
        assert!(got.stdout.is_empty(), "{what}; stdout: {}", stdout(&got));
        assert!(err.contains(&format!("\"code\": {code}")), "{what}");
        assert!(err.contains(want), "{what}; expected {want:?}");
        assert!(
            std::fs::read(&img).expect("read the image") == before,
            "{label} was refused and still changed the image"
        );
        tried.push(label.to_string());
    }
    let labels: Vec<String> = cases.iter().map(|(l, _)| l.to_string()).collect();
    assert_eq!(tried, labels, "the guest answered every case:\n{out}");
}
