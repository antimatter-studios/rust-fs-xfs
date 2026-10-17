#!/usr/bin/env bash
# The random replay oracle remounts to judge every prefix of its sequence.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }

if python3 - "$REPO/tests/random_operations_replay.rs" <<'PY'
import re
import sys
from pathlib import Path

lines = []
for line in Path(sys.argv[1]).read_text().splitlines():
    if not line.startswith("//!"):
        break
    lines.append(line[3:].strip())
intro = " ".join(lines)
errors = []
if re.search(r"at most (?:one|1) checkpoint|(?:one|single)[ -]checkpoint per mount", intro, re.I):
    errors.append("the introduction still claims a one-checkpoint-per-mount limit")
if not all(term in intro for term in ("every prefix", "kernel", "xfs_repair -n", "replay")):
    errors.append("the introduction must explain replay and independent checks of every prefix")
if "many_ops_per_mount.rs" not in intro:
    errors.append("the introduction must point to the repeated-operations oracle")
for error in errors:
    print(f"FAIL  {error}")
sys.exit(bool(errors))
PY
then
    ok 'the random replay introduction explains per-prefix validation'
else
    fail 'the random replay introduction is stale or incomplete'
fi

if [ "$fails" -eq 0 ]; then
    echo 'random-replay-docs: all checks passed'
fi
exit $(( fails > 0 ))
