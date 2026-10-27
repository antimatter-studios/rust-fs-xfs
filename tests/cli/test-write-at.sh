# fs.xfs write --at, as installed, on copies of the kernel-made images: the
# bytes on stdin go into an existing file starting at a byte, in place,
# without truncating it -- inside one block, across a block boundary, over
# several blocks, ending at the end of the file, and in a file whose
# extents are in a B+tree. Every byte before and after the range reads back
# as it was, the size does not change, and the log is left clean.
#
# Every refusal prints nothing on stdout and leaves the image byte-identical:
# exit 3 for a shape the driver does not write in place (past the end, a
# hole), exit 1 for a mistake (a missing file, a directory, a symlink, a
# dirty log), exit 2 for a wrong command line. What xfs_repair and the
# kernel make of a patched image, and an unwritten extent refused, is
# tests/cli_pwrite_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img

v5="$SHARE/xfscli-v5.img"
v4="$SHARE/xfscli-v4.img"
n=0
fresh() {
    n=$((n + 1))
    copy_image "$1" "$SANDBOX/p$n.img"
    printf '%s\n' "$SANDBOX/p$n.img"
}

bs="$(fs.xfs "$v5" get block_size --text)"
check "the v5 image reports a block size ($bs)" test "$bs" -gt 0

# patched BASE PATH AT LEN: on a fresh copy of BASE, LEN random bytes go
# into PATH at byte AT. The tool exits 0 and reports {"path", "at",
# "bytes", "size"}, PATH reads back as it was with exactly that range
# replaced, its size is unchanged, a file beside it is untouched, and the
# log is clean.
patched() {
    local base="$1" path="$2" at="$3" len="$4" tag size rc
    tag="$(basename "$base" .img): write --at $at $path ($len bytes)"
    img="$(fresh "$base")"
    fs.xfs "$img" read "$path" >"$SANDBOX/orig" 2>/dev/null
    size="$(wc -c <"$SANDBOX/orig" | tr -d ' ')"
    head -c "$len" /dev/urandom >"$SANDBOX/patch"
    {
        head -c "$at" "$SANDBOX/orig"
        cat "$SANDBOX/patch"
        tail -c +$((at + len + 1)) "$SANDBOX/orig"
    } >"$SANDBOX/expect"
    fs.xfs "$img" write --at "$at" "$path" <"$SANDBOX/patch" >"$SANDBOX/w.json" 2>"$SANDBOX/w.err"
    rc=$?
    check "$tag exits 0 ($(cat "$SANDBOX/w.err"))" test "$rc" -eq 0
    jq_check "$tag reports the path, position, bytes and size" \
        ".path == \"$path\" and .at == $at and .bytes == $len and .size == $size and (keys | length) == 4" \
        "$SANDBOX/w.json"
    fs.xfs "$img" read "$path" >"$SANDBOX/back" 2>/dev/null
    check "$tag: the file is what it was with only that range replaced" \
        cmp -s "$SANDBOX/expect" "$SANDBOX/back"
    check "$tag: the size is still $size" \
        test "$(fs.xfs "$img" stat "$path" 2>/dev/null | jq -r .size)" = "$size"
    check "$tag: /small.txt beside it is untouched" \
        test "$(fs.xfs "$img" read /small.txt 2>/dev/null)" = "hello world"
    check "$tag leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
}

large=$((8 * 1048576))
patched "$v5" /medium.bin 100 50
patched "$v5" /medium.bin $((bs - 6)) 20
patched "$v5" /medium.bin $((3 * bs + 7)) $((3 * bs))
patched "$v5" /medium.bin 0 262144
patched "$v5" /large.bin $((large - 10)) 10
patched "$v5" /large.bin $((large - bs - 3)) $((bs + 3))
patched "$v5" /fragmented.bin $((2 * bs + 7)) 100
# In place on v4 as well: an overwrite changes no metadata there either.
patched "$v4" /medium.bin $((bs - 6)) 20
patched "$v4" /large.bin $((large - 10)) 10

# The last byte of /small.txt, and its first five, in place.
img="$(fresh "$v5")"
printf '?' | fs.xfs "$img" write --at 11 /small.txt >/dev/null 2>"$SANDBOX/e"
rc=$?
check "write --at 11 /small.txt (its last byte) exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "the last byte of /small.txt is replaced" test "$(fs.xfs "$img" read /small.txt)" = "hello world?"
printf 'HELLO' | fs.xfs "$img" write --at 0 --text /small.txt >"$SANDBOX/text.out" 2>/dev/null
check "write --at --text says what it wrote" \
    test "$(cat "$SANDBOX/text.out")" = "wrote 5 bytes into /small.txt at byte 0"
check "a second write --at on the same copy follows the first" \
    test "$(fs.xfs "$img" read /small.txt)" = "HELLO world?"
check "two writes --at leave the log clean" test "$(fs.xfs "$img" get dirty --text)" = false

# Nothing on stdin writes nothing, anywhere inside the file or at its end.
for at in 0 5 12; do
    img="$(fresh "$v5")"
    copy_image "$img" "$SANDBOX/before.img"
    fs.xfs "$img" write --at "$at" /small.txt </dev/null >"$SANDBOX/w.json" 2>"$SANDBOX/e"
    rc=$?
    check "an empty write --at $at exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
    jq_check "an empty write --at $at reports no bytes" \
        ".at == $at and .bytes == 0 and .size == 12" "$SANDBOX/w.json"
    check "an empty write --at $at left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img"
done

# at_refused CODE PATTERN BASE PATH AT BYTES: on a fresh copy of BASE,
# write --at AT PATH with BYTES random bytes on stdin fails with CODE, an
# error matching PATTERN, nothing on stdout, and the copy unchanged.
at_refused() {
    local code="$1" pattern="$2" base="$3" path="$4" at="$5" bytes="$6" tag rc
    tag="write --at $at $path ($bytes bytes) on $(basename "$base")"
    img="$(fresh "$base")"
    copy_image "$img" "$SANDBOX/before.img"
    head -c "$bytes" /dev/urandom >"$SANDBOX/in"
    fs.xfs "$img" write --at "$at" "$path" <"$SANDBOX/in" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    rc=$?
    check "$tag exits $code ($(cat "$SANDBOX/r.err"))" test "$rc" -eq "$code"
    check "$tag prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "$tag says why" ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/r.err"
    check "$tag left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img"
}

# Shapes the driver does not write in place (exit 3).
at_refused 3 "past the end" "$v5" /small.txt 10 5
at_refused 3 "past the end" "$v5" /small.txt 12 1
at_refused 3 "past the end" "$v5" /small.txt 13 0
at_refused 3 "past the end" "$v5" /empty 0 1
at_refused 3 "past the end" "$v5" /medium.bin 262143 2
at_refused 3 "past the end" "$v4" /small.txt 10 5
at_refused 3 "hole" "$v5" /sparse.bin 0 10
at_refused 3 "hole" "$v5" /sparse.bin $((1280 * bs - 5)) 10
at_refused 3 "hole|unwritten" "$v5" /fragmented.bin $((bs - 1)) 2
at_refused 3 "hole" "$v4" /sparse.bin 0 10
# Mistakes (exit 1).
at_refused 1 "no such file" "$v5" /missing 0 1
at_refused 1 "no such file" "$v5" /missing/f 0 1
at_refused 1 "is a directory" "$v5" /sub 0 1
at_refused 1 "is a directory" "$v5" / 0 1
at_refused 1 "is a symlink" "$v5" /link-short 0 1
at_refused 1 "not a directory" "$v5" /small.txt/ 0 1
at_refused 1 "not a directory" "$v5" /small.txt/x 0 1

# A log holding records is refused before the image is opened for
# writing: the kernel's own volume, shut down while mounted.
dirty_base="$SHARE/xfsdirty.img"
if [ -f "$dirty_base" ]; then
    img="$(fresh "$dirty_base")"
    copy_image "$img" "$SANDBOX/before.img"
    printf x | fs.xfs "$img" write --at 0 /d/renamed-1 >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    check "write --at on a volume whose log holds records exits 1" test $? -eq 1
    check "write --at on a dirty log prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "write --at on a dirty log says the log holds records" \
        '.code == 1 and (.error | test("log holds records"))' "$SANDBOX/x.err"
    check "write --at on a dirty log left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img" "$img"
else
    fail ".vm-share/xfsdirty.img is missing; build it with \`chore fixtures -- data\`"
fi

# A failing producer leaves the image as it was: the whole input is read
# before the image is opened, and a read error is the end of it.
img="$(fresh "$v5")"
copy_image "$img" "$SANDBOX/before.img"
fs.xfs "$img" write --at 0 /medium.bin <"$SANDBOX" >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
check "write --at whose stdin cannot be read exits 1" test $? -eq 1
check "write --at whose stdin cannot be read prints nothing on stdout" test ! -s "$SANDBOX/x.out"
jq_check "write --at whose stdin cannot be read says so" \
    '.code == 1 and (.error | test("read stdin"))' "$SANDBOX/x.err"
check "write --at whose stdin cannot be read left the image as it was" \
    cmp -s "$img" "$SANDBOX/before.img"
rm -f "$SANDBOX/before.img"

# Wrong command lines (exit 2), nothing written.
for bad in -1 x 1.5 ""; do
    img="$(fresh "$v5")"
    copy_image "$img" "$SANDBOX/before.img"
    printf x | fs.xfs "$img" write --at "$bad" /small.txt >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
    check "write --at '$bad' is a usage error (exit 2)" test $? -eq 2
    check "write --at '$bad' prints nothing on stdout" test ! -s "$SANDBOX/u.out"
    check "write --at '$bad' left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img"
done

# --at is the position inside the file; --offset stays the device offset.
disk="$SANDBOX/at-disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
printf 'HELLO' | fs.xfs --offset 1048576 "$disk" write --at 0 /small.txt >/dev/null 2>"$SANDBOX/e"
rc=$?
check "write --at through --offset exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "write --at through --offset patched the file in the partition" \
    test "$(fs.xfs --offset 1048576 "$disk" read /small.txt)" = "HELLO world"
check "write --at through --offset leaves the log clean" \
    test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "the first MiB, outside the partition, is still zeros after write --at" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0

# Plain write is unchanged: without --at, a different length is still the
# whole-file write's refusal, not a patch.
img="$(fresh "$v5")"
printf 'HELLO' | fs.xfs "$img" write /small.txt >/dev/null 2>"$SANDBOX/e"
jq_check "write without --at still refuses a shorter replacement" \
    '.code == 3 and (.error | test("shorter"))' "$SANDBOX/e"
check "write without --at left /small.txt whole" test "$(fs.xfs "$img" read /small.txt)" = "hello world"

finish
