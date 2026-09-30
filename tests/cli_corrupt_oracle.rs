//! A copy the tools refuse is one xfsprogs calls damaged too.
//!
//! `tests/cli/test-read.sh` breaks two things in a copy of the v5 `cli`
//! fixture -- the magic of allocation group 0's AGI, and a byte inside
//! `/small.txt`'s inode core, which leaves its CRC wrong -- and requires
//! every verb that reads them to fail with a structured error and nothing
//! on stdout. A refusal is only right if the image really is damaged, so
//! the same bytes are broken here, at the offsets `xfs_db` gave the fixture
//! builder, and `xfs_repair -n` in the harness guest must call each copy
//! unclean -- and the undamaged fixture clean, so the verdict is about the
//! byte and not about the image.
//!
//! THE TOOL READS A COPY ON THE GUEST'S OWN DISK. Handed a file on the
//! shared folder, `xfs_repair` asks for the underlying filesystem's
//! geometry, gets ENOTDIR, and exits 1 with "cannot open" -- which reads
//! exactly like a verdict of "damaged" and is not one. So each image is
//! copied into the guest's /tmp first, as the write oracles do, and a
//! report without the tool's own phase lines is a failure, not a verdict.

mod cli_support;
mod common;

use cli_support::*;
use common::{fixture, guest_quote, guest_script, repair, scratch};

const SUITE: &str = "cli_corrupt_oracle";

/// The offset the fixture builder recorded for `what`.
fn offset(what: &str) -> u64 {
    let path = fixture("xfscli-v5.corrupt");
    let text = std::fs::read_to_string(&path).expect("read the .corrupt file");
    text.lines()
        .find_map(|l| l.strip_prefix(&format!("{what}\t")))
        .unwrap_or_else(|| panic!("{} names no {what}:\n{text}", path.display()))
        .trim()
        .parse()
        .unwrap_or_else(|e| panic!("{what}'s offset is not a number: {e}"))
}

/// A scratch copy of the v5 fixture with the byte at `at` inverted.
fn damaged(tag: &str, at: u64) -> scratch::Volume {
    use std::io::{Read, Seek, SeekFrom, Write};
    let copy = scratch::Volume::copy_of(
        SUITE,
        &fixture("xfscli-v5.img"),
        &format!("{}-{tag}.img", std::process::id()),
    );
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(copy.path())
        .expect("open the copy");
    let mut byte = [0u8];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.read_exact(&mut byte).unwrap();
    byte[0] = !byte[0];
    f.seek(SeekFrom::Start(at)).unwrap();
    f.write_all(&byte).unwrap();
    f.sync_all().unwrap();
    copy
}

/// What `xfs_repair -n` says about `image`, run on a copy on the guest's
/// own disk: its report, between the markers `repair::script` prints.
fn repair_report(image: &str) -> String {
    let out = guest_script(&format!(
        r#"
        img=$(mktemp -u /tmp/cli-corrupt-XXXXXX.img)
        cp --sparse=always {image} "$img"
        {repair}
        rm -f "$img"
        "#,
        image = guest_quote(image),
        repair = repair::script("\"$img\""),
    ));
    assert!(
        out.ok(),
        "the guest script exited {}:\n{}{}",
        out.status,
        out.stdout,
        out.stderr
    );
    let report = repair::report(&out.stdout);
    assert!(
        report.contains("Phase 1") && !report.contains("cannot open"),
        "xfs_repair did not walk {image}, so it gave no verdict:\n{report}"
    );
    report
}

/// `xfs_repair -n` on `image` must exit non-zero: it found damage.
fn repair_calls_it_unclean(image: &str, what: &str) {
    let report = repair_report(image);
    assert!(
        !report.contains("REPAIR_RC=0"),
        "{what}: xfs_repair -n calls the damaged copy clean, so the tools' refusal is \
         not backed by the reference checker:\n{report}"
    );
}

/// `fs.xfs <image> <verb...>` fails with status 1, a JSON error naming
/// `names`, and nothing on stdout.
fn refused(image: &str, verb: &[&str], names: &str) {
    let out = tool("fs.xfs").arg(image).args(verb).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "{verb:?}: {}", stderr(&out));
    assert!(out.stdout.is_empty(), "{verb:?} wrote to stdout");
    let err = stderr(&out);
    assert!(
        err.starts_with("{\"error\": \"") && err.contains(names),
        "{verb:?}: {err}"
    );
}

#[test]
fn the_undamaged_fixture_is_clean() {
    let report = repair_report(fixture("xfscli-v5.img").to_str().unwrap());
    repair::assert_agreed(&report, "the undamaged xfscli-v5 fixture");
}

#[test]
fn an_inode_that_fails_its_crc_is_refused_by_the_tools_and_by_xfs_repair() {
    let copy = damaged("inode-crc", offset("inode-crc"));
    let image = copy.path().to_str().unwrap();
    refused(image, &["read", "/small.txt"], "CRC");
    refused(image, &["ls", "/"], "CRC");
    repair_calls_it_unclean(image, "an inode core with a wrong CRC");
}

#[test]
fn an_agi_with_a_bad_magic_is_refused_by_the_tools_and_by_xfs_repair() {
    let copy = damaged("agi-magic", offset("agi-magic"));
    let image = copy.path().to_str().unwrap();
    refused(image, &["get"], "AGI");
    refused(image, &["ls", "/"], "AGI");
    refused(image, &["read", "/small.txt"], "AGI");
    repair_calls_it_unclean(image, "an AGI with a bad magic");
}
