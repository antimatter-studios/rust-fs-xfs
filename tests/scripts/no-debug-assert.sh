#!/usr/bin/env bash
#
# no-debug-assert.sh — an invariant that does not run is a comment.
#
# No build that EXECUTES this crate's code is a debug build. CI runs
# `cargo test --locked --release` and nothing else, and the shipped
# artifact is `cargo build --release`; `debug_assert!` compiles out of
# both. The one dev-profile invocation is the pre-commit `cargo check`,
# which compiles without running, so it cannot raise an assertion
# either -- and `[profile.dev]` in Cargo.toml sets only `opt-level` and
# `panic`, so this is not a case of debug assertions having been turned
# off somewhere. There is simply no run in which they are on.
# So a `debug_assert!` guarding what gets written to disk was never an
# invariant at all, and the two that guarded the depth stamped into the
# AGF and AGI went unrun for the whole life of the write path.
#
# The behavioural half of this is tested where it can be reached:
# `group_write::tests::a_buffer_diff_of_mismatched_lengths_is_refused`
# trips a real `assert!` in a release build, and fails without one. But
# most of these guards sit behind an earlier refusal and cannot be
# reached from any argument -- `ag_btree::build` rejects a caller's block
# count upstream with "needs N blocks" -- so no test can trip them, and
# nothing would notice a `debug_assert!` reappearing in one.
#
# Hence a grep. It is a weak assertion about strong-enough evidence: the
# defect class is a form that silently does nothing, and the only durable
# check on a form is its absence.
#
# A `debug_assert!` is not wrong everywhere. It is wrong here, in a crate
# with no debug build, and the honest exemption is `#[cfg(test)]` code
# where the assertion runs under `cargo test` regardless.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

# Code lines only. This file's own subject matter is discussed in
# comments in the sources it checks, and an earlier version of the same
# idea in a sibling repository passed while the call it claimed to check
# had been deleted, because it matched the word in a comment above it.
hits="$(grep -rn 'debug_assert' src/ \
        | grep -vE ':[[:space:]]*(//|/\*|\*)' \
        | grep -vE '^[^:]+:[0-9]+:[[:space:]]*///' || true)"

if [ -n "$hits" ]; then
    echo "FAIL  debug_assert! in a crate that has no debug build:"
    echo "$hits" | sed 's/^/      /'
    echo
    echo "      Make it an \`assert!\` if a panic is the right answer, or"
    echo "      return \`Error::Internal\` if the caller can be told. Both"
    echo "      run in the build that ships."
    exit 1
fi

echo "PASS  no debug_assert! outside comments"
