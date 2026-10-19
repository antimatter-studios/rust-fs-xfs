#!/usr/bin/env bash
#
# vm-setup-download-retry.sh — one dropped connection is not a failed run.
#
# scripts/vm-setup.sh builds the pinned xfsprogs from a kernel.org tarball.
# It used to pipe curl straight into tar, once. The merge-queue run for #547
# lost that download to "curl: (35) Recv failure: Connection reset by
# peer", tar was handed half an archive ("xz: (stdin): File format not
# recognized"), and the guest provision failed before a single test ran
# (#570). Nothing about the change under test was wrong.
#
# So the download is retried a bounded number of times, and what arrives is
# checked against the archive's pinned sha256 before tar sees it. This runs
# a copy of the real vm-setup.sh on the host, with every tool it reaches
# for stubbed on PATH and a curl that drops the first attempts, so it is
# the script's own download step that is under test, not a description of
# it:
#
#   a reset that clears up      the provision finishes from a retry
#   a reset that never clears   the provision fails, bounded, naming the URL
#   a download that is cut      a short archive is refused by its checksum
#   short but exits 0           and fetched again, never handed to tar
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SETUP="$REPO/scripts/vm-setup.sh"
fails=0
ok()   { :; }
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# The archive the stub curl serves: an xfsprogs-<version> directory whose
# configure records that it ran. Its name comes from the script, so a
# version bump does not break this guard.
version="$(sed -n 's/^PARENT_VERSION=\(.*\)$/\1/p' "$SETUP")"
[[ -n "$version" ]] || { echo "FAIL  scripts/vm-setup.sh no longer sets PARENT_VERSION" >&2; exit 1; }
mkdir -p "$tmp/src/xfsprogs-$version"
cat > "$tmp/src/xfsprogs-$version/configure" <<'EOF'
#!/usr/bin/env bash
touch "$STUB_STATE/configured"
EOF
chmod +x "$tmp/src/xfsprogs-$version/configure"
tar -cJf "$tmp/archive.tar.xz" -C "$tmp/src" "xfsprogs-$version"
archive_sha="$(sha256sum "$tmp/archive.tar.xz" | cut -d' ' -f1)"

# The stubs. Everything vm-setup.sh runs that would touch the machine, and
# sleep, so a backoff costs this guard nothing.
mkdir -p "$tmp/bin"
for tool in apt-get modprobe xfs_repair xfs_logprint xfs_io mount losetup sfdisk sleep; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$tmp/bin/$tool"
done
for tool in mkfs.xfs xfs_db; do
    printf '#!/usr/bin/env bash\necho "%s version 6.1.0"\n' "$tool" > "$tmp/bin/$tool"
done
printf '#!/usr/bin/env bash\necho 2\n' > "$tmp/bin/nproc"
cat > "$tmp/bin/make" <<'EOF'
#!/usr/bin/env bash
if [[ "${1:-}" == install ]]; then
    mkdir -p "$STUB_PREFIX/sbin"
    printf '#!/usr/bin/env bash\necho "mkfs.xfs version %s"\n' "$STUB_VERSION" > "$STUB_PREFIX/sbin/mkfs.xfs"
    chmod +x "$STUB_PREFIX/sbin/mkfs.xfs"
fi
EOF
# curl: call N fails with a reset while STUB_CURL_RESETS >= N, delivers a
# truncated archive and exits 0 while STUB_CURL_SHORT >= N, and otherwise
# delivers the archive. Either way it writes where it was asked to: -o FILE,
# or stdout.
cat > "$tmp/bin/curl" <<'EOF'
#!/usr/bin/env bash
out=- url=
while (($#)); do
    case "$1" in
        -o|--output) out="$2"; shift ;;
        http*) url="$1" ;;
    esac
    shift
done
n=$(( $(cat "$STUB_STATE/calls" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$STUB_STATE/calls"
echo "$url" > "$STUB_STATE/url"
deliver() { if [[ "$out" == - ]]; then cat; else cat > "$out"; fi; }
if (( n <= ${STUB_CURL_RESETS:-0} )); then
    head -c 64 "$STUB_ARCHIVE" | deliver
    echo "curl: (35) Recv failure: Connection reset by peer" >&2
    exit 35
fi
if (( n <= ${STUB_CURL_SHORT:-0} )); then
    head -c 64 "$STUB_ARCHIVE" | deliver
    exit 0
fi
deliver < "$STUB_ARCHIVE"
EOF
chmod +x "$tmp/bin/"*

# The copy: the guest's paths moved under $tmp, and the pinned checksum
# swapped for the stub archive's. A substitution that no longer matches is
# a failure, not a guard quietly testing the guest's real paths.
mkdir -p "$tmp/repo/tests/common"
echo "\"$tmp/prefix\"" > "$tmp/repo/tests/common/mod.rs"
sed -e "s|^REPO=/repo\$|REPO=$tmp/repo|" \
    -e "s|^PARENT_PREFIX=/usr/local/xfsprogs-parent\$|PARENT_PREFIX=$tmp/prefix|" \
    -e "s|/var/tmp/xfsprogs\.|$tmp/work.|" \
    -e "s|^PARENT_SHA256=.*\$|PARENT_SHA256=$archive_sha|" \
    "$SETUP" > "$tmp/vm-setup.sh"
for want in "REPO=$tmp/repo" "PARENT_PREFIX=$tmp/prefix" "$tmp/work."; do
    grep -qF "$want" "$tmp/vm-setup.sh" ||
        { echo "FAIL  scripts/vm-setup.sh changed shape: no line became '$want'; update this guard" >&2; exit 1; }
done

# run NAME [VAR=VALUE...]: provision once with a fresh stub state; the exit
# status lands in $tmp/NAME/status and everything printed in $tmp/NAME/log.
run() {
    local name="$1"; shift
    rm -rf "$tmp/prefix"
    mkdir -p "$tmp/$name"
    env PATH="$tmp/bin:$PATH" STUB_STATE="$tmp/$name" STUB_ARCHIVE="$tmp/archive.tar.xz" \
        STUB_PREFIX="$tmp/prefix" STUB_VERSION="$version" "$@" \
        bash "$tmp/vm-setup.sh" > "$tmp/$name/log" 2>&1
    echo $? > "$tmp/$name/status"
}
calls() { cat "$tmp/$1/calls" 2>/dev/null || echo 0; }
said()  { echo "--- vm-setup.sh said:" >&2; sed 's/^/    /' "$tmp/$1/log" >&2; }

# 1. Two resets, then the archive: the provision finishes.
run reset-clears STUB_CURL_RESETS=2
if [[ "$(cat "$tmp/reset-clears/status")" == 0 && -e "$tmp/reset-clears/configured" ]]; then
    ok
else
    fail "two connection resets then a good download failed the provision (curl ran $(calls reset-clears) times)"
    said reset-clears
fi
(( $(calls reset-clears) == 3 )) ||
    fail "two resets then a good download took $(calls reset-clears) curl calls, not 3"

# 2. Resets forever: the provision fails, after a bounded number of tries,
# and says which URL it could not get.
run reset-stays STUB_CURL_RESETS=1000
n="$(calls reset-stays)"
if [[ "$(cat "$tmp/reset-stays/status")" == 0 ]]; then
    fail "a download that never succeeds still let the provision pass"
    said reset-stays
fi
[[ -e "$tmp/reset-stays/configured" ]] && fail "configure ran with no archive downloaded"
(( n >= 3 && n <= 10 )) ||
    fail "a download that never succeeds was tried $n times; want between 3 and 10"
url="$(cat "$tmp/reset-stays/url" 2>/dev/null)"
if [[ -z "$url" ]] || ! grep -qF "$url" "$tmp/reset-stays/log"; then
    fail "the failed provision does not name the URL it could not fetch (${url:-none})"
    said reset-stays
fi

# 3. A cut download that exits 0: refused by its checksum and fetched again,
# never handed to tar.
run short-once STUB_CURL_SHORT=1
if [[ "$(cat "$tmp/short-once/status")" == 0 && -e "$tmp/short-once/configured" ]]; then
    ok
else
    fail "a truncated download that curl reported as success failed the provision instead of being fetched again"
    said short-once
fi
(( $(calls short-once) == 2 )) ||
    fail "one truncated download took $(calls short-once) curl calls, not 2"

# 4. The checksum is pinned in the script, not fetched beside the archive
# from the same server that just failed.
grep -Eq '^PARENT_SHA256=[0-9a-f]{64}$' "$SETUP" ||
    fail "scripts/vm-setup.sh pins no PARENT_SHA256 for the xfsprogs archive"

if (( fails > 0 )); then
    echo "vm-setup-download-retry.sh: $fails check(s) failed" >&2
    exit 1
fi
echo "PASS  vm-setup.sh retries the xfsprogs download and refuses an archive that is not the pinned one"
