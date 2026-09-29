#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

if [[ "${1:-}" == "--print-temp-dir" ]]; then
    exec "$REPO/scripts/with-test-temp.sh" --print-temp-dir
fi

# --features cli: the command-line tools and the tests that run them.
"$REPO/scripts/with-test-temp.sh" cargo test --features cli "$@"
