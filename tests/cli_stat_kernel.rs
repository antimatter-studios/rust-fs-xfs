//! `fs.xfs stat` reports what THE KERNEL reports: a volume made by
//! `mkfs.xfs` (xfsprogs) in the harness VM, filled there by Linux, and
//! every path put to the guest's own `stat`. Identity, type, permission
//! bits, owner, links, size, allocated blocks, device numbers and all four
//! times to the nanosecond must be the ones the kernel printed, with a
//! symlink reported rather than followed.
//!
//! The v4 case is the other half of "a field the format does not
//! store is null": v4 has no creation time. v5 is checked with both the
//! legacy timestamp encoding and bigtime, including a date beyond 2038.

mod cli_support;
mod common;

use cli_support::*;
use common::{guest_quote, kernel_run, oracle, scratch};

/// One line per path: `LABEL|` then the guest's `stat -c FORMAT`.
/// Raw mode in hex, owner, links, size, blocks and their unit, the four
/// times as whole seconds and in the human form whose fraction is the
/// nanoseconds, and the device numbers in hex.
const FORMAT: &str = "%i|%f|%u|%g|%h|%s|%b|%B|%X|%x|%Y|%y|%Z|%z|%W|%w|%t|%T";

fn stat_script(fill: &str, paths: &[&str]) -> String {
    let mut script = format!("cd \"$MNT\"\n{fill}sync\n");
    for path in paths {
        let at = if *path == "/" { "." } else { &path[1..] };
        script.push_str(&format!(
            "printf '%s|' {path}; stat -c '{FORMAT}' -- {at}\n"
        ));
    }
    script
}

/// The nanoseconds in a `stat` human time, `2001-02-03 04:05:06.123456789 +0000`.
fn nanos(human: &str) -> String {
    let fraction = human
        .split_once('.')
        .map(|(_, rest)| rest.split(' ').next().unwrap_or(""))
        .unwrap_or_else(|| panic!("no fraction in the guest's time {human:?}"));
    assert_eq!(fraction.len(), 9, "the guest's time {human:?}");
    fraction
        .trim_start_matches('0')
        .parse::<u64>()
        .unwrap_or(0)
        .to_string()
}

fn kind(raw_mode: u32) -> &'static str {
    match raw_mode & 0o170000 {
        0o100000 => "file",
        0o040000 => "dir",
        0o120000 => "symlink",
        0o020000 => "char",
        0o060000 => "block",
        0o010000 => "fifo",
        0o140000 => "socket",
        _ => "unknown",
    }
}

/// Every field the guest printed for each path, against `fs.xfs stat`.
#[track_caller]
fn assert_agrees(img: &str, report: &str, paths: &[&str], times_stored: bool) {
    let mut seen = Vec::new();
    for line in report.lines() {
        let f: Vec<&str> = line.split('|').collect();
        assert_eq!(f.len(), 19, "a guest stat line: {line}");
        let path = f[0];
        seen.push(path.to_string());
        let ours = stdout(&ok(tool("fs.xfs").args([img, "stat", path])));
        let field = |k: &str| json_field(&ours, k);
        let raw_mode = u32::from_str_radix(f[2], 16).unwrap();
        let what = |k: &str| format!("{path}: {k}\nours:\n{ours}\nthe guest's: {line}");

        assert_eq!(field("path"), path, "{}", what("path"));
        assert_eq!(field("inode"), f[1], "{}", what("inode"));
        assert_eq!(field("type"), kind(raw_mode), "{}", what("type"));
        assert_eq!(
            field("mode"),
            format!("{:04o}", raw_mode & 0o7777),
            "{}",
            what("mode")
        );
        assert_eq!(field("uid"), f[3], "{}", what("uid"));
        assert_eq!(field("gid"), f[4], "{}", what("gid"));
        assert_eq!(field("nlink"), f[5], "{}", what("nlink"));
        assert_eq!(field("size"), f[6], "{}", what("size"));
        let allocated = f[7].parse::<u64>().unwrap() * f[8].parse::<u64>().unwrap();
        assert_eq!(
            field("allocated_bytes"),
            allocated.to_string(),
            "{}",
            what("allocated_bytes")
        );
        for (key, secs, human) in [
            ("atime", f[9], f[10]),
            ("mtime", f[11], f[12]),
            ("ctime", f[13], f[14]),
        ] {
            assert_eq!(field(key), secs, "{}", what(key));
            let nsec = format!("{key}_nsec");
            assert_eq!(field(&nsec), nanos(human), "{}", what(&nsec));
        }
        if times_stored {
            assert_eq!(field("btime"), f[15], "{}", what("btime"));
            assert_eq!(field("btime_nsec"), nanos(f[16]), "{}", what("btime_nsec"));
        } else {
            assert_eq!(f[16], "-", "{}", what("btime"));
            assert_eq!(field("btime"), "null", "{}", what("btime"));
            assert_eq!(field("btime_nsec"), "null", "{}", what("btime_nsec"));
        }
        match kind(raw_mode) {
            "char" | "block" => {
                let major = u32::from_str_radix(f[17], 16).unwrap();
                let minor = u32::from_str_radix(f[18], 16).unwrap();
                assert_eq!(field("major"), major.to_string(), "{}", what("major"));
                assert_eq!(field("minor"), minor.to_string(), "{}", what("minor"));
            }
            "symlink" => assert!(ours.contains("\"target\": "), "{}", what("target")),
            _ => assert!(!ours.contains("\"target\": "), "{}", what("target")),
        }
    }
    assert_eq!(
        seen, paths,
        "the guest printed a line for each path:\n{report}"
    );
}

fn agrees(name: &str, options: &str, creation: bool, bigtime: bool) {
    let image = scratch::Volume::empty("cli-stat-kernel", name, 320 << 20);
    // Tools and mounts use a copy on the guest's disk, never a loop device
    // backed by the shared folder. Unmount before copying the final bytes back.
    let fill = "mkdir dir\nchmod 1777 dir\n\
                head -c 5000 /dev/urandom > file\nchown 1234:5678 file\n\
                chmod 0640 file\ntouch -m -d '2001-02-03 04:05:06.123456789' file\n\
                ln file hard\n: > empty\nprintf x > old\n\
                touch -d '1960-01-02 03:04:05.5' old\nln -s file link\n\
                mkfifo fifo\nmknod chr c 1 3\nmknod blk b 7 300\n\
                mknod wide c 300 70000\nln file $'bad-\\377'\n\
                xfs_io -c 'chproj 100000' -c 'chattr +ds' file\n";
    let fill = format!(
        "{fill}{}",
        if bigtime {
            "touch -m -d '2100-01-02 03:04:05.987654321' file\n"
        } else {
            ""
        }
    );
    let paths = [
        "/", "/dir", "/file", "/hard", "/empty", "/old", "/link", "/fifo", "/chr", "/blk", "/wide",
    ];
    let report = kernel_run(&format!(
        r#"
        sync; echo 3 > /proc/sys/vm/drop_caches
        work=$(mktemp -d)
        trap 'mountpoint -q "$work/m" && umount "$work/m"; rm -rf "$work"' EXIT
        truncate -s 320M "$work/image"
        mkfs.xfs -q -f {options} "$work/image"
        mkdir "$work/m"
        mount -o loop,nouuid "$work/image" "$work/m"
        MNT="$work/m"
        ( {script} )
        umount "$work/m"
        cp --sparse=always "$work/image" {image}
        sync
        echo DONE
        "#,
        script = stat_script(&fill, &paths),
        image = guest_quote(image.path().to_str().unwrap()),
    ));
    let report = report
        .lines()
        .filter(|l| l.starts_with('/'))
        .collect::<Vec<_>>()
        .join("\n");
    let img = image.path().to_str().unwrap();
    assert_agrees(img, &report, &paths, creation);
    let file = stdout(&ok(tool("fs.xfs").args([img, "stat", "/file"])));
    assert_eq!(json_field(&file, "projid"), "100000", "{file}");
    assert!(file.contains("\"NODUMP\""), "{file}");
    assert!(file.contains("\"SYNC\""), "{file}");
    assert_eq!(file.contains("\"BIGTIME\""), bigtime, "{file}");
    let raw_name = std::ffi::OsStr::new(img);
    use std::os::unix::ffi::OsStrExt;
    let raw = stdout(&ok(tool("fs.xfs")
        .arg(raw_name)
        .arg("stat")
        .arg(std::ffi::OsStr::from_bytes(b"/bad-\xff"))));
    assert_eq!(json_field(&raw, "path_hex"), "2f6261642dff", "{raw}");
    assert_eq!(json_field(&raw, "inode"), json_field(&file, "inode"));
    let link = stdout(&ok(tool("fs.xfs").args([img, "stat", "/link"])));
    assert_eq!(json_field(&link, "target"), "file", "{link}");
    assert_eq!(json_field(&link, "size"), "4", "{link}");

    // The format-specific fields are held to xfsprogs on the same inode.
    let ino = json_field(&file, "inode");
    let db = oracle("xfs_db")
        .args([
            "-r",
            "-c",
            &format!("inode {ino}"),
            "-c",
            "print core.version core.gen core.extsize core.nextents core.anextents core.format",
            img,
        ])
        .output();
    assert!(db.ok(), "{}", db.stderr);
    for (key, field) in [
        ("version", "version"),
        ("generation", "gen"),
        ("extsize", "extsize"),
        ("nextents", "nextents"),
        ("anextents", "anextents"),
    ] {
        let prefix = format!("core.{field} = ");
        let value = db
            .stdout
            .lines()
            .find_map(|l| l.strip_prefix(&prefix))
            .expect(&db.stdout);
        assert_eq!(
            json_field(&file, key),
            value,
            "{key}: {file}\n{}",
            db.stdout
        );
    }
    assert_eq!(json_field(&file, "format"), "extents", "{file}");
    assert!(
        db.stdout.contains("core.format = 2 (extents)"),
        "{}",
        db.stdout
    );
}

#[test]
fn fs_xfs_stat_reports_what_the_kernel_reports() {
    agrees("v5.img", "-m crc=1,bigtime=0", true, false);
}

#[test]
fn fs_xfs_stat_reports_bigtime_beyond_2038() {
    agrees("bigtime.img", "-m crc=1,bigtime=1", true, true);
}

#[test]
fn fs_xfs_stat_reports_null_for_creation_time_v4_cannot_hold() {
    agrees("v4.img", "-m crc=0 -i projid32bit=1", false, false);
}
