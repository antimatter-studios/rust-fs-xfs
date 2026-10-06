#!/usr/bin/env bash
# test-cli.sh — THE `cli` TIER: the command-line tools as installed, found
# on PATH, not a cargo target.
#
# The same suite checks a checkout's build (`chore cli:install` stages it and
# prints the PATH line) and a Homebrew install (`brew install
# antimatter-studios/tap/rust-fs-xfs`): it tests whatever PATH resolves,
# because that is what a user runs.
#
# STEP 1, BEFORE ANY TOOL IS TESTED: the tools are present and are ours.
#   - jq and rust-fs-xfs must resolve, and rust-fs-xfs must answer
#     --version as this crate;
#   - `rust-fs-xfs doctor` must pass: every dotted name on PATH is our
#     program at our version. If one is shadowed -- a distribution's
#     /usr/sbin/fs.xfs, another formula's, an older install of ours --
#     the tier fails with doctor's report: what wins, and the fix.
# NOTHING SKIPS. A missing tool fails the tier naming what provides it; a
# tier that tested someone else's fs.xfs and reported green would be
# worse than no tier.
#
# AND THE IMAGES ARE THERE. This crate has no mkfs, so the images the
# files read are made by mkfs.xfs and filled by the kernel in the harness
# guest: the `cli` fixture set, built by `chore fixtures`. A missing one
# fails the tier naming that task, before any file runs.
#
# WHAT IS NOT HERE: the independent oracle. What xfsprogs and the kernel
# make of the tools' reads and writes is judged by tests/cli_*_oracle.rs
# and tests/cli_*_kernel.rs, in the oracle and kernel tiers, where every
# call reaches the guest through tests/common and fails, never skips,
# when it cannot.
#
# STEP 2: every tests/cli/test-*.sh, by glob, so a new one needs no edit
# here. Each prints its failures, a `test result: ok. N passed; ...` line
# (the count `scripts/ci-test.sh --gate` reads, as it does cargo's), and LAST
# `<name>: all checks passed`. A file that exits 0 without that last line
# stopped early and is a failure: `exit 0` part-way through is not evidence
# a file finished.
#
# Quiet: the tier runs under ../rust-fs-core/scripts/tier.sh, which keeps the whole run in
# tmp/logs/cli.log.
#
# CLI_TESTS names another directory of test-*.sh files, and CLI_SHARE
# another fixture directory, for tests/scripts/test-cli-tier.sh, which
# proves every refusal above refuses.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd -P)"
TESTS="${CLI_TESTS:-$REPO/tests/cli}"
SHARE="${CLI_SHARE:-$REPO/.vm-share}"
CRATE="$(sed -n 's/^name *= *"\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -n 1)"
INSTALL="build and stage it with \`chore cli:install\` (it prints the PATH line to use), or install it with \`brew install antimatter-studios/tap/rust-fs-xfs\`"

refuse() {
    echo "test-cli: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 ||
    refuse "jq is not on PATH; the suite reads every JSON report with it. Install it: \`brew install jq\`, or \`apt-get install jq\`."

entry="$(command -v rust-fs-xfs 2>/dev/null || true)"
[ -n "$entry" ] || refuse "rust-fs-xfs is not on PATH: $INSTALL."
answer="$(rust-fs-xfs --version 2>/dev/null || true)"
case "$answer" in
    "rust-fs-xfs ($CRATE) "*) ;;
    *) refuse "$entry is not $CRATE's rust-fs-xfs: --version answered '$answer'. Put ours first on PATH: $INSTALL." ;;
esac
echo "== $answer, at $entry"

echo "== rust-fs-xfs doctor"
if ! report="$(rust-fs-xfs doctor --text 2>&1)"; then
    printf '%s\n' "$report" >&2
    refuse "doctor found a tool on PATH that is not this program; its fixes are above. Nothing was tested."
fi
printf '%s\n' "$report"

for image in xfscli-v5.img xfscli-v4.img; do
    [ -f "$SHARE/$image" ] ||
        refuse ".vm-share/$image is missing: the images are made by mkfs.xfs and filled by the kernel in the harness VM. Build them with \`chore fixtures -- cli\`. Nothing was tested."
done

shopt -s nullglob
files=("$TESTS"/test-*.sh)
[ "${#files[@]}" -gt 0 ] || refuse "no test-*.sh in $TESTS to run"

failed=0
for file in "${files[@]}"; do
    name="$(basename "$file" .sh)"
    echo "== $name"
    output="$(bash "$file" 2>&1)"
    status=$?
    printf '%s\n' "$output"
    last="$(printf '%s\n' "$output" | tail -n 1)"
    if [ "$status" -ne 0 ]; then
        echo "FAIL  $name exited $status" >&2
        failed=$((failed + 1))
    elif [ "$last" != "$name: all checks passed" ]; then
        echo "FAIL  $name exited 0 without its last line, '$name: all checks passed': it stopped early" >&2
        failed=$((failed + 1))
    fi
done

if [ "$failed" -gt 0 ]; then
    echo "test-cli: $failed of ${#files[@]} files failed" >&2
    exit 1
fi
echo "test-cli: ${#files[@]} files, all checks passed"
