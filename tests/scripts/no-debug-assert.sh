#!/usr/bin/env bash
#
# no-debug-assert.sh — an invariant that does not run is a comment.
#
# The build that ships is `cargo build --release`, and `debug_assert!`
# compiles out of it. CI's suites run `--release` too; the one debug run
# is ci.yml's `cargo test --lib` overflow gate, which exercises the
# library's unit tests and not a volume anyone writes. So a
# `debug_assert!` guarding what gets written to disk is not an invariant
# of the code users run, and the two that guarded the depth stamped into
# the AGF and AGI went unrun in every shipped build of the write path.
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
#
# The exemption is implemented, not just stated (#136): a line inside an
# item carrying `#[cfg(test)]` -- a `mod tests { ... }`, a test-only
# helper `fn` -- is not a hit. The item is found by brace depth from the
# first `{` after the attribute, with `//` comments stripped first. That
# is a scanner, not a parser: a `{` or `}` inside a string literal in a
# test module would miscount. `self_test` below pins the shapes it does
# handle, and runs before the real scan so a broken scanner cannot pass.

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO"

# The code lines naming debug_assert under DIR, outside #[cfg(test)]
# items, as FILE:LINE:TEXT.
#
# Code lines only. This file's own subject matter is discussed in
# comments in the sources it checks, and an earlier version of the same
# idea in a sibling repository passed while the call it claimed to check
# had been deleted, because it matched the word in a comment above it.
scan() {
    local dir="$1"
    find "$dir" -name '*.rs' -type f -print0 | sort -z | xargs -0 -r awk '
        FNR == 1 { pending = 0; depth = 0 }
        {
            code = $0
            sub(/\/\/.*/, "", code)
            opens = gsub(/\{/, "{", code)
            closes = gsub(/\}/, "}", code)
            if (depth > 0) {
                depth += opens - closes
                next
            }
            if (code ~ /^[ \t]*#\[cfg\(test\)\]/) {
                pending = 1
                sub(/^[ \t]*#\[cfg\(test\)\]/, "", code)
                if (code !~ /\{/) next
            }
            if (pending) {
                if (code ~ /\{/) {
                    pending = 0
                    depth = opens - closes
                    next
                }
                if (code ~ /;[ \t]*$/) pending = 0
                next
            }
            if ($0 ~ /debug_assert/ && $0 !~ /^[ \t]*(\/\/|\/\*|\*)/) {
                print FILENAME ":" FNR ":" $0
            }
        }
    '
}

# The scanner against sources whose answers are known.
self_test() {
    local t
    t="$(mktemp -d)"
    trap 'rm -rf "$t"' RETURN
    cat > "$t/shipped.rs" <<'RS'
fn write() {
    debug_assert!(depth < 8);
}
RS
    cat > "$t/exempt.rs" <<'RS'
/// debug_assert in a doc comment
// debug_assert in a comment
fn real() {}

#[cfg(test)]
mod tests {
    fn helper() {
        if true { debug_assert!(1 < 2); }
    }
    #[test]
    fn t() {
        debug_assert!(true); // a `}` in a comment does not close the module
    }
}

#[cfg(test)]
#[allow(dead_code)]
fn test_only_helper() {
    debug_assert!(true);
}

#[cfg(test)] mod inline { fn f() { debug_assert!(true); } }
RS
    cat > "$t/after.rs" <<'RS'
#[cfg(test)]
mod tests {
    #[test]
    fn t() {}
}

fn shipped_after_the_test_module() {
    debug_assert!(false);
}

#[cfg(test)]
use std::fmt;

fn shipped_after_a_test_only_use() {
    debug_assert!(false);
}
RS
    local got want
    got="$(scan "$t" | sed "s#^$t/##" | cut -d: -f1,2)"
    want="after.rs:8
after.rs:15
shipped.rs:2"
    if [ "$got" != "$want" ]; then
        echo "FAIL  the scanner's self-test: expected"
        echo "$want" | sed 's/^/      /'
        echo "      got"
        echo "${got:-<nothing>}" | sed 's/^/      /'
        exit 1
    fi
    echo "PASS  the scanner exempts #[cfg(test)] items and nothing else"
}

self_test

hits="$(scan src)"

if [ -n "$hits" ]; then
    echo "FAIL  debug_assert! in code the shipped build runs:"
    echo "$hits" | sed 's/^/      /'
    echo
    echo "      Make it an \`assert!\` if a panic is the right answer, or"
    echo "      return \`Error::Internal\` if the caller can be told. Both"
    echo "      run in the build that ships."
    exit 1
fi

echo "PASS  no debug_assert! outside comments and #[cfg(test)] items"
