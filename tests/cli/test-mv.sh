# fs.xfs mv, as installed, on copies of the kernel-made images: a rename in
# place, moves across directories both ways past short form, a directory
# move, and a file and a directory replacing what was at PATH. After each
# move SOURCE no longer resolves and PATH is what SOURCE was -- the same
# inode, type and size, a file's bytes, a directory's children -- and the
# volume is released clean, so moves chain on one copy.
#
# Every refusal prints nothing on stdout and leaves the image byte-identical:
# exit 1 for a mistake, with the reason Linux gives; exit 3 for a v4 volume
# and for a replacement the driver does not do. What xfs_repair and the
# kernel make of a move, and that each mistake is one the kernel refuses for
# the same reason, is tests/cli_mv_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img

v5="$SHARE/xfscli-v5.img"
v4="$SHARE/xfscli-v4.img"
n=0
fresh() {
    n=$((n + 1))
    copy_image "$1" "$SANDBOX/m$n.img"
    printf '%s\n' "$SANDBOX/m$n.img"
}

# look PATH: what fs.xfs sees at PATH, its name aside. A directory's
# children by name, type, size, inode and target; anything else's type,
# size, inode and target, and a checksum of the bytes `read` gives.
look() {
    if [ "$(fs.xfs "$img" stat "$1" 2>/dev/null | jq -r .type)" = dir ]; then
        fs.xfs "$img" ls "$1" 2>&1 | jq -c 'map([.name, .type, .size, .inode, .target])'
    else
        fs.xfs "$img" ls "$1" 2>&1 | jq -c 'map([.type, .size, .inode, .target])'
        fs.xfs "$img" read "$1" 2>/dev/null | cksum
    fi
}

# nlink PATH: the link count stat reports.
nlink() {
    fs.xfs "$img" stat "$1" 2>/dev/null | jq -r .nlink
}

# moved SOURCE PATH: mv exits 0 and reports {"path": PATH, "moved": true},
# SOURCE is gone, PATH is what SOURCE was, and the log is clean.
moved() {
    local source="$1" path="$2" rc
    look "$source" >"$SANDBOX/was"
    fs.xfs "$img" mv "$source" "$path" >"$SANDBOX/mv.json" 2>"$SANDBOX/mv.err"
    rc=$?
    check "mv $source $path exits 0 ($(cat "$SANDBOX/mv.err"))" test "$rc" -eq 0
    jq_check "mv $source $path reports the new path moved" \
        ".path == \"$path\" and .moved == true and (keys | length) == 2" "$SANDBOX/mv.json"
    fs.xfs "$img" stat "$source" >/dev/null 2>"$SANDBOX/gone.err"
    jq_check "after mv $source $path the old name is gone" \
        '.code == 1 and (.error | test("no such file"))' "$SANDBOX/gone.err"
    look "$path" >"$SANDBOX/now"
    check "after mv $source $path the new name is what the old one was" \
        cmp -s "$SANDBOX/was" "$SANDBOX/now"
    check "mv $source $path leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
}

img="$(fresh "$v5")"

# A rename in place, in a directory in its inode.
moved /small.txt /renamed.txt
check "the renamed file still reads its bytes" test "$(fs.xfs "$img" read /renamed.txt)" = "hello world"

# Across directories: into a short-form directory, out of a directory past
# short form, and into one.
moved /medium.bin /sub/medium.bin
moved /manyfiles/entry-7.txt /sub/entry-7.txt
check "the file moved out of /manyfiles still reads its bytes" test "$(fs.xfs "$img" read /sub/entry-7.txt)" = 7
moved /large.bin /manyfiles/large.bin
fs.xfs "$img" ls /manyfiles >"$SANDBOX/ls.json" 2>/dev/null
jq_check "/manyfiles lost entry-7.txt and gained large.bin" \
    'length == 200 and all(.[]; .name != "entry-7.txt") and any(.[]; .name == "large.bin")' "$SANDBOX/ls.json"

# A directory moved to another: its `..` goes with it, the old parent loses
# the link that `..` was and the new parent gains it.
root_links="$(nlink /)"
sub_links="$(nlink /sub)"
moved /modes /sub/modes
check "the root lost a link with /modes ($root_links, then $(nlink /))" test "$(nlink /)" -eq $((root_links - 1))
check "/sub gained a link with /modes ($sub_links, then $(nlink /sub))" test "$(nlink /sub)" -eq $((sub_links + 1))
check "a file in the moved directory still reads its bytes" test "$(fs.xfs "$img" read /sub/modes/m4755)" = 4755

# A directory past short form renamed in place.
moved /manyfiles /many

# A file replacing a file: the name it replaced is freed with its blocks.
free_before="$(fs.xfs "$img" get free_bytes --text)"
moved /renamed.txt /sub/medium.bin
free_after="$(fs.xfs "$img" get free_bytes --text)"
check "replacing the 256 KiB /sub/medium.bin gave its blocks back ($free_before, then $free_after)" \
    test $((free_after - free_before)) -ge 262144

# A directory replacing an empty one, which goes with the link its `..` was.
fs.xfs "$img" rm /sub/nested/file.txt >/dev/null 2>"$SANDBOX/e"
fs.xfs "$img" ls /sub/nested >"$SANDBOX/ls.json" 2>>"$SANDBOX/e"
jq_check "rm /sub/nested/file.txt empties /sub/nested ($(cat "$SANDBOX/e"))" 'length == 0' "$SANDBOX/ls.json"
sub_links="$(nlink /sub)"
moved /sub/modes /sub/nested
check "/sub lost the replaced directory's link ($sub_links, then $(nlink /sub))" test "$(nlink /sub)" -eq $((sub_links - 1))

fs.xfs "$img" mv --text /sub/entry-7.txt /entry-7.txt >"$SANDBOX/text.out" 2>/dev/null
check "mv --text says what it moved" test "$(cat "$SANDBOX/text.out")" = "moved /sub/entry-7.txt to /entry-7.txt"
fs.xfs "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the root holds every name the moves left there" \
    '([.[].name] | sort) == (["empty", "entry-7.txt", "fragmented.bin", "link-short", "many", "sparse.bin", "sub"] | sort)' \
    "$SANDBOX/ls.json"
fs.xfs "$img" ls /sub >"$SANDBOX/ls.json" 2>/dev/null
jq_check "/sub holds every name the moves left there" \
    '([.[].name] | sort) == (["link-remote", "medium.bin", "nested"] | sort)' "$SANDBOX/ls.json"

# mv_refused CODE PATTERN BASE SOURCE PATH: on a fresh copy of BASE, mv
# exits CODE with PATTERN in the error, nothing on stdout, and the copy
# byte for byte as it was.
mv_refused() {
    local code="$1" pattern="$2" base="$3" source="$4" path="$5" rc what
    what="mv $source $path on $(basename "$base")"
    img="$(fresh "$base")"
    copy_image "$img" "$SANDBOX/before.img"
    fs.xfs "$img" mv "$source" "$path" </dev/null >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    rc=$?
    check "$what exits $code ($(cat "$SANDBOX/r.err"))" test "$rc" -eq "$code"
    check "$what prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "$what says why" ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/r.err"
    check "$what left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img" "$img"
}

# Mistakes (exit 1), each for the reason Linux gives.
mv_refused 4 "no such file" "$v5" /missing /x
mv_refused 4 "no such file" "$v5" /missing/x /y
mv_refused 4 "no such file" "$v5" /small.txt /missing/x
mv_refused 4 "not a directory" "$v5" /small.txt /empty/x
mv_refused 4 "not a directory" "$v5" /small.txt/ /x
mv_refused 4 "not a directory" "$v5" /small.txt /x/
mv_refused 4 "not a directory" "$v5" /sub /small.txt
mv_refused 4 "not a directory" "$v5" /sub /link-short
mv_refused 1 "is a directory" "$v5" /small.txt /sub
mv_refused 4 "not empty" "$v5" /modes /sub
mv_refused 4 "not empty" "$v5" /sub/nested /sub
mv_refused 1 "invalid argument" "$v5" /sub /sub/x
mv_refused 1 "invalid argument" "$v5" /sub /sub/nested/x
mv_refused 1 "cannot be moved or replaced" "$v5" / /x
mv_refused 1 "cannot be moved or replaced" "$v5" /sub/. /x
mv_refused 1 "cannot be moved or replaced" "$v5" /sub/.. /x
mv_refused 1 "cannot be moved or replaced" "$v5" /small.txt /sub/.
mv_refused 1 "cannot be moved or replaced" "$v5" /small.txt /
mv_refused 1 "file name too long" "$v5" /small.txt "/$(printf 'n%.0s' $(seq 1 256))"
# A v4 volume, and a replacement the driver does not do (exit 3).
mv_refused 3 "v4 filesystem is not supported" "$v4" /small.txt /renamed.txt
mv_refused 3 "v4 filesystem is not supported" "$v4" /modes /sub/modes
mv_refused 3 "B.tree" "$v5" /small.txt /fragmented.bin

# A log holding records nothing has applied is refused, as every write
# verb refuses it. No verb leaves such a log, so the volume is the kernel's
# own, shut down while mounted (`chore fixtures -- data` builds it).
dirty_base="$SHARE/xfsdirty.img"
if [ -f "$dirty_base" ]; then
    img="$(fresh "$dirty_base")"
    copy_image "$img" "$SANDBOX/before.img"
    fs.xfs "$img" mv /d/renamed-1 /d/moved </dev/null >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
    check "mv on a volume whose log holds records exits 1" test $? -eq 1
    check "mv on a dirty log prints nothing on stdout" test ! -s "$SANDBOX/x.out"
    jq_check "mv on a dirty log says the log holds records" \
        '.code == 1 and (.error | test("log holds records"))' "$SANDBOX/x.err"
    check "mv on a dirty log left the image as it was" cmp -s "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img" "$img"
else
    fail ".vm-share/xfsdirty.img is missing; build it with \`chore fixtures -- data\`"
fi

img="$(fresh "$v5")"
fs.xfs "$img" mv /small.txt >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "mv with one path is a usage error (exit 2)" test $? -eq 2
check "mv with one path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# --offset reaches a partition for moving too, and nothing before it.
disk="$SANDBOX/mv-disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
fs.xfs --offset 1048576 "$disk" mv /small.txt /sub/small.txt >/dev/null 2>"$SANDBOX/e"
rc=$?
check "mv through --offset exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "mv through --offset moved the file" \
    test "$(fs.xfs --offset 1048576 "$disk" read /sub/small.txt)" = "hello world"
check "mv through --offset leaves the log clean" \
    test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "the first MiB, outside the partition, is still zeros after mv" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0

finish
