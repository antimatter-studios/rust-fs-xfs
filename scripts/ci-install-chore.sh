#!/usr/bin/env bash
#
# ci-install-chore.sh — install the pinned chore on a CI runner.
#
# Every CI job runs chore tasks (the siblings, the tools, the fixtures,
# the tests), so chore comes first, pinned by CHORE_VERSION (set in the
# workflow) and checked against the release's own checksums.
#
# LINUX AND macOS, because this repository's gate includes a job on the
# target it ships to: aarch64-apple-darwin, where ENOTSUP is 45 rather
# than 95 and `c_char` is unsigned. That job runs chore tasks like every
# other, so it needs chore, and a Linux-only installer would have made it
# the one job that reproduced nothing a developer can run.
set -euo pipefail

: "${CHORE_VERSION:?set CHORE_VERSION, e.g. 0.11.0}"
case "$(uname -s)" in
    Linux) os=linux ;;
    Darwin) os=darwin ;;
    *) echo "ci-install-chore: no chore build for $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
    x86_64 | amd64) arch=x86_64 ;;
    aarch64 | arm64) arch=arm64 ;;
    *) echo "ci-install-chore: no chore build for $(uname -m)" >&2; exit 1 ;;
esac
# macOS has no sha256sum; `shasum -a 256` reads the same format.
if command -v sha256sum >/dev/null 2>&1; then
    sha256_check() { sha256sum -c -; }
else
    sha256_check() { shasum -a 256 -c -; }
fi
tarball="chore-${CHORE_VERSION}-${os}-${arch}.tar.gz"
base="https://github.com/antimatter-studios/chore/releases/download/v${CHORE_VERSION}"
dir="${RUNNER_TEMP:-$(mktemp -d)}/chore"
mkdir -p "$dir"
curl -fsSL -o "$dir/$tarball" "$base/$tarball"
curl -fsSL -o "$dir/checksums.txt" "$base/checksums.txt"
(cd "$dir" && grep " ${tarball}\$" checksums.txt | sha256_check)
tar -xzf "$dir/$tarball" -C "$dir"
bin="$(find "$dir" -type f -name chore -perm -u+x | head -1)"
[ -n "$bin" ] || { echo "ci-install-chore: no chore binary in $tarball" >&2; exit 1; }
install -D -m 0755 "$bin" "$HOME/.local/bin/chore"
[ -z "${GITHUB_PATH:-}" ] || echo "$HOME/.local/bin" >> "$GITHUB_PATH"
"$HOME/.local/bin/chore" --version | head -1
