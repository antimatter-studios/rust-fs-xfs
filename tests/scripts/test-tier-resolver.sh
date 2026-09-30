#!/usr/bin/env bash
# scripts/tier.sh runs rust-fs-core's output-budget.sh only once that script
# has identified itself (#254).
#
# Existence is not identity. A stale vendor directory, a half-written file or
# a package that gutted the script all leave a path that exists and does not
# behave -- and a wrapper that ignores its arguments and runs the command makes
# a tier "pass" with no budget, no log and no verdict. So tier.sh asks the
# script for `--version` and refuses anything that does not answer
# `rust-fs-core-output-budget 1`.
#
# A refusal nobody executes has never been shown to happen, so each one is
# driven here, through FS_CORE_ROOT, which names core outright and has no
# fallback:
#
#   - FS_CORE_ROOT naming a directory with no wrapper: refused, naming
#     rust-fs-core, and the command never runs;
#   - a wrapper that answers --version with something else: refused, saying
#     what it checked, and the command never runs -- even though cargo could
#     have found a good copy elsewhere;
#   - a wrapper that answers correctly: the tier runs through it, so the
#     refusals are not a resolver that always fails.
#
# No build and no network: FS_CORE_ROOT is authoritative, so cargo is never
# asked.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
TIER="$REPO/scripts/tier.sh"

fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/tier-resolver.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT HUP INT TERM

# The command every tier below is asked to run. Its marker appearing is the
# evidence that the resolver let a wrapper run it.
ran="$sandbox/ran"
cmd=(bash -c "touch '$ran'")

# --- A core that is not there. -------------------------------------------
rm -f "$ran"
out="$(FS_CORE_ROOT="$sandbox/nowhere" bash "$TIER" t resolver-none 10 1000 -- "${cmd[@]}" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] || fail "tier.sh ran a tier with FS_CORE_ROOT naming no wrapper"
[ ! -e "$ran" ] || fail "the command ran although no wrapper was found"
case "$out" in *rust-fs-core*) ;; *) fail "the missing-core refusal did not name rust-fs-core: $out" ;; esac

# --- A core that is there and is not core's. -------------------------------
# It ignores its arguments and runs whatever follows `--`: exactly the shape
# that turns a tier green with no budget enforced.
mkdir -p "$sandbox/wrong/scripts"
cat >"$sandbox/wrong/scripts/output-budget.sh" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = "--version" ]; then echo "some-other-wrapper 9"; exit 0; fi
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
exec "$@"
STUB
rm -f "$ran"
out="$(FS_CORE_ROOT="$sandbox/wrong" bash "$TIER" t resolver-wrong 10 1000 -- "${cmd[@]}" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] || fail "tier.sh accepted a wrapper that is not rust-fs-core's"
[ ! -e "$ran" ] || fail "the command ran through a wrapper that did not identify itself"
case "$out" in *--version*) ;; *) fail "the wrong-core refusal did not say what it checked: $out" ;; esac

# --- A wrapper with nothing in it. -----------------------------------------
mkdir -p "$sandbox/empty/scripts"
: >"$sandbox/empty/scripts/output-budget.sh"
rm -f "$ran"
out="$(FS_CORE_ROOT="$sandbox/empty" bash "$TIER" t resolver-empty 10 1000 -- "${cmd[@]}" 2>&1)"
rc=$?
[ "$rc" -ne 0 ] || fail "tier.sh accepted an empty output-budget.sh"
[ ! -e "$ran" ] || fail "the command ran although the wrapper was empty"

# --- A wrapper that identifies itself. -------------------------------------
mkdir -p "$sandbox/right/scripts"
cat >"$sandbox/right/scripts/output-budget.sh" <<'STUB'
#!/usr/bin/env bash
if [ "${1:-}" = "--version" ]; then echo "rust-fs-core-output-budget 1"; exit 0; fi
while [ $# -gt 0 ] && [ "$1" != "--" ]; do shift; done
shift
exec "$@"
STUB
rm -f "$ran"
out="$(FS_CORE_ROOT="$sandbox/right" bash "$TIER" t resolver-right 10 1000 -- "${cmd[@]}" 2>&1)"
rc=$?
[ "$rc" -eq 0 ] || fail "a wrapper that answered --version correctly was refused ($rc): $out"
[ -e "$ran" ] || fail "the command did not run through a wrapper that identified itself"

if [ "$fails" -gt 0 ]; then
    echo "FAIL  $fails tier resolver check(s)" >&2
    exit 1
fi
echo "PASS  tier.sh runs only a wrapper that identifies itself as rust-fs-core's"
