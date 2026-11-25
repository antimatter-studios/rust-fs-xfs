#!/usr/bin/env bash
# Print Cargo's resolved target directory, honoring environment and config.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
cargo metadata --locked --format-version 1 --no-deps "$@" \
    | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])'
