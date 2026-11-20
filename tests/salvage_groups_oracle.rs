//! A group's inode and free-space B+trees, walked for salvage, against what
//! `xfs_db` prints for the same trees, in the harness guest.
//!
//! On a fixture whose trees are one leaf deep, `addr root` then `print recs`
//! is every record of the tree, decoded by xfsprogs. The salvage walk has to
//! return the same records.

mod common;

use common::{fixture, oracle};
use fs_core::FileDevice;
use fs_xfs::salvage;

/// Records from `print recs` (`recs[1-2] = [startblock,blockcount]`, then
/// `1:[10,6]`, `2:[24,25576]`), as the field names and each record's values.
fn records(text: &str) -> (Vec<String>, Vec<Vec<u64>>) {
    // The header line names the fields; the records follow it, one per
    // line or several to a line.
    let at = text
        .find("recs[")
        .unwrap_or_else(|| panic!("xfs_db printed no records:\n{text}"));
    let joined = text[at..].split_whitespace().collect::<Vec<_>>().join(" ");
    let (_, rest) = joined.split_once(" = ").unwrap();
    let (names, values) = rest.split_once(']').unwrap();
    let names = names
        .trim_start_matches('[')
        .split(',')
        .map(str::to_string)
        .collect();
    let values = values
        .split_whitespace()
        .filter_map(|record| {
            let (_, inside) = record.split_once(":[")?;
            Some(
                inside
                    .trim_end_matches(']')
                    .split(',')
                    .map(|v| match v.strip_prefix("0x") {
                        Some(hex) => u64::from_str_radix(hex, 16).unwrap(),
                        None => v.parse().unwrap(),
                    })
                    .collect(),
            )
        })
        .collect();
    (names, values)
}

fn field(names: &[String], record: &[u64], name: &str) -> u64 {
    let at = names
        .iter()
        .position(|n| n == name)
        .unwrap_or_else(|| panic!("no field {name} in {names:?}"));
    record[at]
}

/// Every group's free-space and inode records, on a v5 and a v4 fixture,
/// are the records `xfs_db` prints.
#[test]
fn group_trees_agree_with_xfs_db() {
    for name in ["xfs-default", "xfs-nocrc"] {
        let image = fixture(&format!("{name}.img"));
        let device = FileDevice::open(&image).unwrap();
        let sb = salvage::superblocks(&device).geometry.unwrap();
        for ag in 0..sb.agcount {
            let headers = salvage::ag_headers(&device, &sb, ag);
            let agf = headers.agf.value.unwrap();
            let agi = headers.agi.value.unwrap();
            assert_eq!(
                agf.levels[0], 1,
                "{name} AG {ag}: the test needs a one-leaf tree"
            );
            assert_eq!(
                agi.level, 1,
                "{name} AG {ag}: the test needs a one-leaf tree"
            );

            let out = oracle("xfs_db")
                .args([
                    "-r",
                    "-c",
                    &format!("agf {ag}"),
                    "-c",
                    "addr bnoroot",
                    "-c",
                    "print recs",
                ])
                .arg(image.to_str().unwrap())
                .output();
            assert!(
                out.ok(),
                "{name}: xfs_db failed:\n{}{}",
                out.stdout,
                out.stderr
            );
            let (names, theirs) = records(&out.stdout);
            let ours = salvage::free_extents(&device, &sb, ag, &agf);
            assert!(ours.damage.is_empty());
            let ours: Vec<Vec<u64>> = ours
                .records
                .iter()
                .map(|e| vec![u64::from(e.startblock), u64::from(e.blockcount)])
                .collect();
            let theirs: Vec<Vec<u64>> = theirs
                .iter()
                .map(|r| {
                    vec![
                        field(&names, r, "startblock"),
                        field(&names, r, "blockcount"),
                    ]
                })
                .collect();
            assert_eq!(ours, theirs, "{name} AG {ag} free space:\n{}", out.stdout);

            let out = oracle("xfs_db")
                .args([
                    "-r",
                    "-c",
                    &format!("agi {ag}"),
                    "-c",
                    "addr root",
                    "-c",
                    "print recs",
                ])
                .arg(image.to_str().unwrap())
                .output();
            assert!(
                out.ok(),
                "{name}: xfs_db failed:\n{}{}",
                out.stdout,
                out.stderr
            );
            let ours = salvage::inode_chunks(&device, &sb, ag, &agi);
            assert!(ours.damage.is_empty());
            if ours.records.is_empty() {
                assert!(
                    !out.stdout.contains("recs["),
                    "{name} AG {ag}: xfs_db lists chunks the walk did not find:\n{}",
                    out.stdout
                );
                continue;
            }
            let (names, theirs) = records(&out.stdout);
            let ours: Vec<Vec<u64>> = ours
                .records
                .iter()
                .map(|c| vec![u64::from(c.startino), u64::from(c.freecount), c.free])
                .collect();
            let theirs: Vec<Vec<u64>> = theirs
                .iter()
                .map(|r| {
                    vec![
                        field(&names, r, "startino"),
                        field(&names, r, "freecount"),
                        field(&names, r, "free"),
                    ]
                })
                .collect();
            assert_eq!(ours, theirs, "{name} AG {ag} inode chunks:\n{}", out.stdout);
        }
    }
}
