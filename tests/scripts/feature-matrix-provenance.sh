#!/usr/bin/env bash
# Run the builder's refusal paths with hermetic formatter stand-ins.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
ok() { printf 'ok    %s\n' "$1"; }
fail() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }
mkdir -p "$REPO/tmp"
sandbox="$(mktemp -d "$REPO/tmp/feature-provenance.XXXXXX")"
mkdir -p "$sandbox/bin" "$sandbox/images"
for tool in mkfs.xfs xfs_info xfs_db xfs_quota xfs_repair; do
    cat > "$sandbox/bin/$tool" <<'SH'
#!/usr/bin/env bash
if [ "${1:-}" = -V ]; then
    echo "${0##*/} version ${MOCK_XFS_VERSION:-6.13.0}"
    exit 0
fi
echo 'formatter deliberately rejected the recipe' >&2
exit 42
SH
    chmod +x "$sandbox/bin/$tool"
done

if MOCK_XFS_VERSION=6.12.0 XFS_MATRIX_XFSPROGS_BIN="$sandbox/bin" \
    XFS_FIXTURE_DIR="$sandbox/images" bash "$REPO/scripts/build-feature-matrix-fixtures.sh" > "$sandbox/version.log" 2>&1; then
    fail 'wrong formatter version is refused'
elif grep -q 'must be version 6.13.0' "$sandbox/version.log"; then
    ok 'wrong formatter version is refused before image construction'
else
    fail 'wrong version failed for an unrelated reason'
fi

if XFS_MATRIX_XFSPROGS_BIN="$sandbox/bin" XFS_FIXTURE_DIR="$sandbox/images" \
    XFS_FIXTURE_SIZE=1M bash "$REPO/scripts/build-feature-matrix-fixtures.sh" > "$sandbox/recipe.log" 2>&1; then
    fail 'a rejected recipe fails the matrix build'
elif grep -q 'v4 rejected by pinned mkfs.xfs' "$sandbox/recipe.log" \
    && grep -q 'formatter deliberately rejected the recipe' "$sandbox/recipe.log" \
    && ! grep -q 'SKIP' "$sandbox/recipe.log"; then
    ok 'a rejected recipe fails and preserves the formatter diagnostic'
else
    fail 'recipe refusal failed for an unrelated reason'
fi

# Exercise the complete successful control flow without claiming Linux-format
# validation: formatter and kernel calls are stand-ins, the evidence files real.
mkdir -p "$sandbox/success-bin" "$sandbox/success-images"
for tool in mkfs.xfs xfs_info xfs_db xfs_quota xfs_repair; do
    cat > "$sandbox/success-bin/$tool" <<'SH'
#!/usr/bin/env bash
if [ "${1:-}" = -V ]; then
    echo "${0##*/} version 6.13.0"
else
    if [ "${0##*/}" = mkfs.xfs ] && [[ " $* " == *" su=64k,sw=4"* ]]; then
        # Model this fixture's pinned 6.13 constraints, not a preferred recipe.
        # mkfs/xfs_mkfs.c validates redundancy and the minimum log, then
        # bounds the log by libxfs_alloc_ag_max_usable() and stripe alignment.
        groups=8
        log_mib=0
        for arg in "$@"; do
            case "$arg" in
                su=64k,sw=4,agcount=*) groups="${arg##*agcount=}" ;;
                size=*m) log_mib="${arg#size=}"; log_mib="${log_mib%m}" ;;
            esac
        done
        if [ "$groups" -lt 2 ]; then
            echo 'Filesystem must have at least 2 superblocks for redundancy!' >&2
            exit 42
        fi
        if [ "$log_mib" -lt 64 ]; then
            echo 'Log size must be at least 64MB.' >&2
            exit 42
        fi
        # Fixed 400 MiB, 4 KiB blocks, 64 KiB stripe-unit geometry. The
        # 28-block margin matches the pinned formatter's observed cap
        # (12772 blocks in a 50 MiB AG), including alignment/reservations.
        max_logblocks=$((400 * 256 / groups - 28))
        logblocks=$((log_mib * 256))
        if [ "$logblocks" -gt "$max_logblocks" ]; then
            echo "internal log size $logblocks too large, must be less than $max_logblocks" >&2
            exit 42
        fi
    fi
    printf '%s evidence for %s UID 1001\n' "${0##*/}" "${*: -1}"
    [ "${0##*/}" != xfs_db ] || echo 'rootino = 128'
fi
SH
done
for tool in mount umount sync dd rmdir chown; do
    printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/success-bin/$tool"
done
printf '#!/usr/bin/env bash\nexit 1\n' > "$sandbox/success-bin/cp"
printf '#!/usr/bin/env bash\necho 8\n' > "$sandbox/success-bin/stat"
printf '#!/usr/bin/env bash\necho 0\n' > "$sandbox/success-bin/id"
chmod +x "$sandbox/success-bin/"*

for probe in '1:64:at least 2 superblocks' '4:32:at least 64MB' '8:64:too large' '4:128:too large'; do
    groups="${probe%%:*}"; rest="${probe#*:}"
    log_mib="${rest%%:*}"; expected="${rest#*:}"
    if "$sandbox/success-bin/mkfs.xfs" -f -m crc=1 \
        -d "su=64k,sw=4,agcount=$groups" -l "size=${log_mib}m" \
        "$sandbox/images/constraint.img" > "$sandbox/constraint.log" 2>&1; then
        fail "formatter model accepted invalid stripe constraint $probe"
    elif ! grep -q "$expected" "$sandbox/constraint.log"; then
        fail "formatter model gave an unrelated refusal for $probe"
    fi
done
for groups in 2 4; do
    if ! "$sandbox/success-bin/mkfs.xfs" -f -m crc=1 \
        -d "su=64k,sw=4,agcount=$groups" -l size=64m \
        "$sandbox/images/constraint.img" > "$sandbox/constraint.log" 2>&1; then
        fail "formatter model refused a fitting 64 MiB log with $groups AGs"
    fi
done

if TMPDIR="$sandbox" PATH="$sandbox/success-bin:$PATH" XFS_MATRIX_XFSPROGS_BIN="$sandbox/success-bin" \
    XFS_FIXTURE_DIR="$sandbox/success-images" XFS_FIXTURE_SIZE=1M \
    bash "$REPO/scripts/build-feature-matrix-fixtures.sh" > "$sandbox/success.log" 2>&1; then
    # Derive the expected rows from the actual recipes rather than a second list.
    names="$(sed -nE 's/^[[:space:]]*"([^:]+):.*"/\1/p' "$REPO/scripts/build-feature-matrix-fixtures.sh")"
    complete=1
    for name in $names; do
        for suffix in img mkfs info sbdump rootdump repair provenance; do
            [ -s "$sandbox/success-images/xfsfeat-$name.$suffix" ] || complete=0
        done
        provenance="$sandbox/success-images/xfsfeat-$name.provenance"
        if [ -f "$provenance" ]; then
            grep -q "^fixture=xfsfeat-$name.img$" "$provenance" || complete=0
            [ "$(grep -c 'version 6.13.0' "$provenance")" -eq 5 ] || complete=0
            grep -q '^recipe=mkfs.xfs -f ' "$provenance" || complete=0
            grep -Eq '^[0-9a-f]{64}  ' "$provenance" || complete=0
        fi
    done
    [ -s "$sandbox/success-images/xfsfeat-meta_uuid.uuid" ] || complete=0
    [ -s "$sandbox/success-images/xfsfeat-quota.quota" ] || complete=0
    if [ "$complete" -eq 1 ]; then
        ok 'every successful recipe retains its own formatter and final-image evidence'
    else
        fail 'successful recipes lost their row identity or provenance'
    fi
else
    fail 'successful control-flow stand-ins failed to build the matrix'
    tail -8 "$sandbox/success.log"
fi

if [ "$fails" -eq 0 ]; then
    echo 'feature-matrix-provenance: all checks passed'
fi
exit $(( fails > 0 ))
