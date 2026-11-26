#!/usr/bin/env bash
#
# max-rss.sh — run one test program and record its peak resident memory, for the
# before-and-after figures of #606. CI sets it as cargo's runner; it is removed
# before this change merges.
set -euo pipefail
mkdir -p "$(dirname "${MAX_RSS_LOG:?max-rss.sh needs MAX_RSS_LOG}")"
exec /usr/bin/time -a -o "$MAX_RSS_LOG" -f '%M KiB %C' "$@"
