# Installed truncate: sizes, released blocks, zero-filled growth, clean
# release and byte-identical refusals. Linux judges it in cli_truncate_kernel.
source "$(dirname "$0")/lib.sh"
need_fixture xfscli-v5.img xfscli-v4.img xfsdirty.img
v5="$SHARE/xfscli-v5.img"
img="$SANDBOX/truncate.img"
copy_image "$v5" "$img"
fs.xfs "$img" read /medium.bin >"$SANDBOX/original"
free_before="$(fs.xfs "$img" get free_bytes --text)"

fs.xfs "$img" truncate /medium.bin 4196 >"$SANDBOX/report" 2>"$SANDBOX/e"
check "cross-block shrink succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
jq_check "truncate reports only path and byte size" \
    '.path == "/medium.bin" and .size == 4196 and (keys | length) == 2' "$SANDBOX/report"
head -c 4196 "$SANDBOX/original" >"$SANDBOX/expected"
fs.xfs "$img" read /medium.bin >"$SANDBOX/read"
check "shrink preserves the prefix" cmp -s "$SANDBOX/expected" "$SANDBOX/read"
check "shrink releases blocks" test "$(fs.xfs "$img" get free_bytes --text)" -gt "$free_before"
check "shrink releases clean" test "$(fs.xfs "$img" get dirty --text)" = false

fs.xfs "$img" truncate /medium.bin 16384 >"$SANDBOX/report" 2>"$SANDBOX/e"
check "regrowth succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
jq_check "regrowth reports the new size" '.size == 16384' "$SANDBOX/report"
dd if=/dev/zero bs=1 count=12188 status=none >>"$SANDBOX/expected"
fs.xfs "$img" read /medium.bin >"$SANDBOX/read"
check "regrowth exposes zeros instead of discarded bytes" cmp -s "$SANDBOX/expected" "$SANDBOX/read"
check "regrowth releases clean" test "$(fs.xfs "$img" get dirty --text)" = false

for size in 4096 0; do
    fs.xfs "$img" truncate /medium.bin "$size" >"$SANDBOX/report" 2>"$SANDBOX/e"
    check "shrink to $size succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
    jq_check "shrink reports $size" ".size == $size" "$SANDBOX/report"
    head -c "$size" "$SANDBOX/original" >"$SANDBOX/expected"
    fs.xfs "$img" read /medium.bin >"$SANDBOX/read"
    check "shrink to $size reads exactly that prefix" cmp -s "$SANDBOX/expected" "$SANDBOX/read"
    check "shrink to $size releases clean" test "$(fs.xfs "$img" get dirty --text)" = false
done
fs.xfs "$img" stat /medium.bin >"$SANDBOX/stat"
jq_check "zero shrink frees every data block" '.size == 0 and .blocks == 0' "$SANDBOX/stat"
fs.xfs "$img" truncate --text /medium.bin 8197 >"$SANDBOX/text" 2>"$SANDBOX/e"
check "growth from zero succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
check "text reports the length" test "$(cat "$SANDBOX/text")" = '/medium.bin is 8197 bytes'
dd if=/dev/zero of="$SANDBOX/expected" bs=1 count=8197 status=none
fs.xfs "$img" read /medium.bin >"$SANDBOX/read"
check "growth from zero reads zeros" cmp -s "$SANDBOX/expected" "$SANDBOX/read"
fs.xfs "$img" truncate /medium.bin 8197 >"$SANDBOX/report" 2>"$SANDBOX/e"
check "same-size truncate succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
check "same-size truncate stays clean" test "$(fs.xfs "$img" get dirty --text)" = false

refused() {
    local code="$1" pattern="$2" base="$3"
    shift 3
    copy_image "$base" "$SANDBOX/refused.img"
    fs.xfs "$SANDBOX/refused.img" truncate "$@" >"$SANDBOX/out" 2>"$SANDBOX/e"
    check "truncate $* exits $code ($(cat "$SANDBOX/e"))" test $? -eq "$code"
    check "truncate $* prints no stdout" test ! -s "$SANDBOX/out"
    jq_check "truncate $* explains refusal" ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/e"
    check "truncate $* leaves image byte-identical" cmp -s "$base" "$SANDBOX/refused.img"
}
refused 1 'not a regular file' "$v5" /sub 0
refused 1 'not a regular file' "$v5" /link-short 0
refused 1 'no such file' "$v5" /missing 0
refused 1 'not a directory' "$v5" /small.txt/ 0
refused 1 'not a directory' "$v5" /link-short/child 0
refused 3 'B\\+tree' "$v5" /fragmented.bin 1000000
refused 3 'v4 filesystem is not supported' "$SHARE/xfscli-v4.img" /small.txt 0
refused 3 'size|length' "$v5" /small.txt 18446744073709551615
refused 1 'log holds records' "$SHARE/xfsdirty.img" /whatever 0
for size in junk 1K 1.5 -1 18446744073709551616; do
    refused 2 '.' "$v5" /small.txt "$size"
done
refused 2 'required' "$v5" /small.txt
# Parsing must precede even opening a nonexistent target.
fs.xfs "$SANDBOX/nonexistent" truncate /f junk >"$SANDBOX/out" 2>"$SANDBOX/e"
check "invalid size precedes opening TARGET" test $? -eq 2
jq_check "invalid size has the usage envelope" '.code == 2' "$SANDBOX/e"
check "invalid size prints no stdout" test ! -s "$SANDBOX/out"

disk="$SANDBOX/disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
fs.xfs --offset 1048576 "$disk" truncate /small.txt 5 >"$SANDBOX/report" 2>"$SANDBOX/e"
check "truncate through --offset succeeds ($(cat "$SANDBOX/e"))" test $? -eq 0
check "offset selects the filesystem" test "$(fs.xfs --offset 1048576 "$disk" read /small.txt)" = hello
check "offset truncate releases clean" test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "truncate leaves the device prefix untouched" test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0
finish
