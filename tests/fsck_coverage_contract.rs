//! Every checker code is damaged by a case `xfs_repair -n` agrees with,
//! or says why not (#364).
//!
//! `docs/fsck-output.md` lists the codes under `## Codes` and, under
//! `## Damage coverage`, the case that damages each. This holds the two
//! tables to each other and to the tests they name: a code the catalogue
//! has and the coverage table does not, a case the table names in a file
//! that never mentions the code, and a code with neither a case nor a
//! reason all fail here. It needs no tool and no fixture, so a pull
//! request adding a code without its damage fails the unit tier.

use std::collections::BTreeMap;

fn doc() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/fsck-output.md"))
        .expect("docs/fsck-output.md")
}

/// The first two cells of every row of the table under `heading`, the
/// first with its backquotes taken off.
fn table(doc: &str, heading: &str) -> BTreeMap<String, String> {
    let start = doc
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no {heading} in docs/fsck-output.md"));
    let rest = &doc[start + heading.len() + 2..];
    let end = rest.find("\n## ").unwrap_or(rest.len());
    rest[..end]
        .lines()
        .filter(|l| l.starts_with("| `"))
        .map(|l| {
            let cells: Vec<&str> = l.split('|').map(str::trim).collect();
            (cells[1].trim_matches('`').to_string(), cells[2].to_string())
        })
        .collect()
}

#[test]
fn every_code_has_damage_or_a_reason() {
    let doc = doc();
    let codes = table(&doc, "## Codes");
    let coverage = table(&doc, "## Damage coverage");
    assert!(codes.len() > 40, "the code table was not read: {codes:?}");

    let missing: Vec<&String> = codes
        .keys()
        .filter(|c| !coverage.contains_key(*c))
        .collect();
    assert!(
        missing.is_empty(),
        "codes with no damage-coverage row: {missing:?}"
    );
    let unknown: Vec<&String> = coverage
        .keys()
        .filter(|c| !codes.contains_key(*c))
        .collect();
    assert!(
        unknown.is_empty(),
        "coverage rows for codes the catalogue does not have: {unknown:?}"
    );

    for (code, case) in &coverage {
        if let Some(reason) = case.strip_prefix("none:") {
            assert!(
                reason.trim().len() > 30,
                "{code}: no damage case, and no reason worth the name: {case:?}"
            );
            continue;
        }
        let file = case
            .split('`')
            .nth(1)
            .unwrap_or_else(|| panic!("{code}: {case:?} names no test file"));
        let path = format!("{}/tests/{file}", env!("CARGO_MANIFEST_DIR"));
        let source = std::fs::read_to_string(&path)
            .unwrap_or_else(|_| panic!("{code}: {file} does not exist"));
        if case.ends_with("by finding text") {
            continue;
        }
        assert!(
            source.contains(&format!("\"{code}\"")),
            "{code}: {file} never names the code, so it cannot be checking it"
        );
        let name = case.rsplit(": ").next().unwrap_or_default();
        if !name.contains(' ') {
            assert!(
                source.contains(&format!("\"{name}\"")),
                "{code}: {file} has no case {name:?}"
            );
        }
    }
}
