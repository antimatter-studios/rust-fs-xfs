#!/usr/bin/env bash
# Exercise the oracle's report reader against xfsprogs' numeric-ID format.
# xfsprogs v6.1.0 quota/report.c report_row prints IDs with "#%-10u".
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
python3 - "$REPO/tests/quota_accounting_oracle.rs" <<'PY'
import pathlib
import subprocess
import sys

source = pathlib.Path(sys.argv[1]).read_text()
start = source.index("        report() {{")
end = source.index("        report 0", start)
report = source[start:end].replace("{{", "{").replace("}}", "}")
# Independent report rows: headers, an unrelated UID, zero usage, and limits
# different from usage ensure the reader selects both the right row and column.
stub = r'''
set -euo pipefail
m=/quota-report
xfs_quota() {
    case "$3" in
        'report -u -b -n')
            printf '%s\n' 'User quota on /quota-report' 'User ID Used Soft Hard Warn/Grace' \
                '#655340 99 100 101 00 [--------]' \
                '#0 12 20 24 00 [--------]' '#65534 0 0 4 00 [--------]'
            ;;
        'report -u -i -n')
            printf '%s\n' 'User quota on /quota-report' 'User ID Used Soft Hard Warn/Grace' \
                '#655340 77 78 79 00 [--------]' \
                '#0 18 30 40 00 [--------]' '#65534 1 0 0 00 [--------]'
            ;;
        *) exit 1 ;;
    esac
}
'''
result = subprocess.run(
    ["bash", "-c", stub + report + "\nreport 0\nreport 65534\n"],
    capture_output=True, text=True, check=True,
)
expected = ["USAGE_0 12 18", "USAGE_65534 0 1"]
actual = result.stdout.splitlines()
if actual != expected:
    print(f"FAIL numeric quota reports: expected {expected!r}, got {actual!r}", file=sys.stderr)
    sys.exit(1)
print("PASS root and nobody quota usage accept xfsprogs numeric IDs")
PY
status=$?
[ "$status" -eq 0 ] && echo "quota-report-numeric-ids: all checks passed"
exit "$status"
