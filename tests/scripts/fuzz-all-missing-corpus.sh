#!/usr/bin/env bash
#
# fuzz-all-missing-corpus.sh — a target with no seed corpus is reported
# as missing, not as a crash.
#
# The scheduled fuzz run said "fuzzing found crashes in: log_dinode
# dir_block_form extent_list" every day for a week. None of the three
# had crashed: none of them had a directory under fuzz/corpus/, libFuzzer
# printed "No such file or directory ... exiting" and returned 1, and
# scripts/fuzz-all.sh read every non-zero exit as a finding. Three
# parsers were never fuzzed, and the report said the opposite of why.
#
# So this runs the real script in a sandbox, against a stand-in `cargo`
# that behaves the way libFuzzer does -- exit 1 on a corpus directory
# that is not there, write an artifact and exit 1 on a crash -- and
# checks what the script says about each case:
#
#   - a missing corpus fails the run, naming the directory, and is never
#     called a crash;
#   - the missing target is not handed to the fuzzer at all;
#   - a target with a corpus is still fuzzed, so one gap does not stop
#     the rest of the night's run;
#   - a genuine crash is still reported as one;
#   - a non-zero exit that left no artifact is a run that failed, not a
#     finding.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

sandbox="$(mktemp -d "${TMPDIR:-/tmp}/fuzz-all-test.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

mkdir -p "$sandbox/repo/scripts" "$sandbox/repo/fuzz/corpus/seeded" \
         "$sandbox/repo/fuzz/corpus/crashes" "$sandbox/repo/fuzz/corpus/broken" \
         "$sandbox/repo/fuzz/corpus/emptied" "$sandbox/bin"
cp "$REPO/scripts/fuzz-all.sh" "$sandbox/repo/scripts/fuzz-all.sh"
cat > "$sandbox/repo/fuzz/Cargo.toml" <<'EOF'
[package]
name = "sandbox-fuzz"

[[bin]]
name = "seeded"

[[bin]]
name = "unseeded"

[[bin]]
name = "emptied"

[[bin]]
name = "crashes"

[[bin]]
name = "broken"
EOF
for t in seeded crashes broken; do
    printf 'seed' > "$sandbox/repo/fuzz/corpus/$t/one.bin"
done

# A stand-in for `cargo +nightly fuzz run <target> <dir>...`, faithful to
# the three exits that matter: libFuzzer's on a missing corpus directory,
# a crash (which leaves an artifact), and a build that failed (which
# leaves nothing).
cat > "$sandbox/bin/cargo" <<'EOF'
#!/usr/bin/env bash
target="$4"
echo "$target" >> "$FUZZ_TEST_LOG"
shift 4
for arg in "$@"; do
    [ "$arg" = "--" ] && break
    if [ ! -d "$arg" ]; then
        echo "INFO: libFuzzer ignores flags that start with '--'"
        echo "No such file or directory: $arg; exiting"
        exit 1
    fi
done
case "$target" in
    crashes)
        mkdir -p "fuzz/artifacts/$target"
        printf 'boom' > "fuzz/artifacts/$target/crash-0000"
        exit 1 ;;
    broken)
        echo "error: could not compile" >&2
        exit 101 ;;
esac
exit 0
EOF
chmod +x "$sandbox/bin/cargo"

export FUZZ_TEST_LOG="$sandbox/invoked"
: > "$FUZZ_TEST_LOG"
out="$(PATH="$sandbox/bin:$PATH" bash "$sandbox/repo/scripts/fuzz-all.sh" 1 2>&1)"
status=$?

[ "$status" -ne 0 ] || fail "a run with a missing corpus exited 0"

grep -q "fuzz/corpus/unseeded" <<<"$out" \
    || fail "the missing directory fuzz/corpus/unseeded is not named"
grep -q "fuzz/corpus/emptied" <<<"$out" \
    || fail "the empty directory fuzz/corpus/emptied is not named"
if grep -E "'(unseeded|emptied)' found an input" <<<"$out" >/dev/null \
   || grep -E "crashes in:.*(unseeded|emptied)" <<<"$out" >/dev/null; then
    fail "a target with no seed corpus was reported as a crash"
fi
if grep -qx -e unseeded -e emptied "$FUZZ_TEST_LOG"; then
    fail "a target with no seed corpus was handed to the fuzzer"
fi
grep -qx seeded "$FUZZ_TEST_LOG" \
    || fail "a target with a corpus was not fuzzed because another had none"
grep -q "seeded: clean" <<<"$out" \
    || fail "the seeded target is not reported clean"
grep -Eq "crashes in:.* crashes( |$)" <<<"$out" \
    || fail "a genuine crash is no longer reported as a crash"
if grep -Eq "crashes in:.*broken" <<<"$out"; then
    fail "a run that failed without leaving an artifact was reported as a crash"
fi
grep -q "'broken'" <<<"$out" \
    || fail "a run that failed without an artifact is not named"

if [ "$fails" -gt 0 ]; then
    echo "--- scripts/fuzz-all.sh said:" >&2
    echo "$out" >&2
    exit 1
fi
echo "PASS  fuzz-all.sh tells a missing corpus and a failed run apart from a crash"
