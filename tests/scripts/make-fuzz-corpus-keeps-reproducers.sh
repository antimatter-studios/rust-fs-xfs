#!/usr/bin/env bash
#
# make-fuzz-corpus-keeps-reproducers.sh — rebuilding the seed corpus
# leaves every file it did not generate where it was.
#
# fuzz/corpus/ is not all generated. Alongside the seeds cut out of a
# mkfs.xfs image sit the reproducers for defects the fuzzer found --
# fuzz/corpus/superblock/regression-log-field-masked-shift.bin is one --
# committed there because scripts/fuzz-all.sh says to, so that
# tests/fuzz_decoders.rs replays them on every pull request. The corpus
# script used to start with `rm -rf fuzz/corpus`, so regenerating the
# seeds silently threw every reproducer away (#258).
#
# So this runs the real script in a sandbox, against stand-ins for the
# tools it drives -- mkfs.xfs, xfs_db, the python3 that finds blocks by
# their magic, and the cargo that writes the derived seeds -- with a
# reproducer already committed, and checks:
#
#   - the reproducer is still there afterwards, byte for byte;
#   - the script ran to the end: it exits 0 and the seeds it generates
#     were written, so a run that stopped early cannot pass by never
#     reaching the line that would have deleted anything;
#   - a stale generated seed is replaced, not left as it was.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
fails=0
fail() { echo "FAIL  $*" >&2; fails=$((fails + 1)); }

sandbox="$(mktemp -d "${TMPDIR:-/tmp}/make-fuzz-corpus-test.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT

mkdir -p "$sandbox/repo/scripts" "$sandbox/repo/fuzz/corpus/superblock" "$sandbox/bin"
cp "$REPO/scripts/make-fuzz-corpus.sh" "$sandbox/repo/scripts/make-fuzz-corpus.sh"

reproducer="$sandbox/repo/fuzz/corpus/superblock/regression-log-field-masked-shift.bin"
printf 'a committed reproducer' > "$reproducer"
stale="$sandbox/repo/fuzz/corpus/superblock/mkfs-crc-rmapbt-reflink.bin"
printf 'stale' > "$stale"

# mkfs.xfs: the image is already the size asked for, and its contents do
# not matter here -- only which files end up under fuzz/corpus.
printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/bin/mkfs.xfs"
# xfs_db: every `daddr` lookup lands on sector 16.
printf '#!/usr/bin/env bash\necho "current daddr is 16"\n' > "$sandbox/bin/xfs_db"
# python3: both scans "find" every block and inode they look for.
cat > "$sandbox/bin/python3" <<'EOF'
#!/usr/bin/env bash
cat >/dev/null
for name in dir_data bmbt dir_leaf dir_node inode_dir inode_file inode_symlink; do
    echo "$name=24"
done
EOF
# cargo: the derived-seed writer, which has nothing to derive from here.
printf '#!/usr/bin/env bash\nexit 0\n' > "$sandbox/bin/cargo"
chmod +x "$sandbox/bin/"*

out="$(PATH="$sandbox/bin:$PATH" bash "$sandbox/repo/scripts/make-fuzz-corpus.sh" 1M 2>&1)"
status=$?

[ "$status" -eq 0 ] || fail "the script exited $status with every tool it needs stood in"
for seed in superblock/mkfs-crc-rmapbt-reflink.bin bmbt/mkfs-bmbt-block.bin \
            inode/symlink-local-format.bin; do
    [ -s "$sandbox/repo/fuzz/corpus/$seed" ] \
        || fail "the generated seed fuzz/corpus/$seed was not written"
done
if [ ! -f "$reproducer" ]; then
    fail "regenerating the corpus deleted the committed reproducer superblock/regression-log-field-masked-shift.bin"
elif [ "$(cat "$reproducer")" != 'a committed reproducer' ]; then
    fail "regenerating the corpus rewrote the committed reproducer"
fi
if printf 'stale' | cmp -s - "$stale"; then
    fail "a generated seed was left stale instead of being regenerated"
fi

if [ "$fails" -gt 0 ]; then
    echo "--- scripts/make-fuzz-corpus.sh said:" >&2
    echo "$out" >&2
    exit 1
fi
echo "PASS  make-fuzz-corpus.sh regenerates its own seeds and keeps committed reproducers"
