//! `rust-fs-xfs`: the command-line tools for XFS, one multi-call binary.
//!
//! Installed as `rust-fs-xfs` and linked as each dotted name; see
//! `common` for the dispatch and the output contract every tool shares,
//! and `xfs` for the tools themselves.
//!
//! There is one dotted name, `fs.xfs`. No `mkfs.xfs` and no `fsck.xfs`:
//! this crate has no initial-layout builder and no checker, and a missing
//! link is how a package says a tool is not there.

// The shared plumbing is a library in waiting (see its module docs): its
// API is whole, and a piece XFS does not call yet is not dead, it is the
// part another driver's tools will.
#[allow(dead_code)]
mod common;
mod xfs;

use std::process::ExitCode;

static FAMILY: common::Family = common::Family {
    repo: "rust-fs-xfs",
    crate_name: env!("CARGO_PKG_NAME"),
    version: env!("CARGO_PKG_VERSION"),
    about: "XFS tools: work on an XFS image or device directly, without mounting it",
    install_hints: &[
        "`chore cli:install` from a checkout of this repository",
        "`brew install antimatter-studios/tap/rust-fs-xfs`",
    ],
    tools: &[xfs::fs::TOOL],
};

fn main() -> ExitCode {
    common::main(&FAMILY)
}
