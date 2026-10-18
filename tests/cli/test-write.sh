# fs.xfs write and mkdir, as installed, on copies of the kernel-made
# images: every shape the driver can write round-trips byte for byte, and
# every shape it cannot is refused with its reason -- exit 3 for a shape,
# exit 1 for a mistake -- with nothing on stdout and the copy unchanged.
#
# A JOURNALLED WRITE RELEASES THE VOLUME CLEAN. A new file, a filled empty
# file and a mkdir are log records, and the driver writes only to a volume
# whose log is clean; each verb then writes its change home and ends the
# log with an unmount record, so `get dirty` is false afterwards and the
# next journalled write follows on the same copy with no kernel between.
# What xfs_repair and the kernel make of such a chain is
# tests/cli_write_kernel.rs.
#
# rm AND rmdir RELEASE THE VOLUME CLEAN TOO, so removals chain on one copy
# as the writes above do, and a volume whose log holds records is refused.
# What xfs_repair and the kernel make of a removal is tests/cli_rm_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img

v5="$SHARE/xfscli-v5.img"
v4="$SHARE/xfscli-v4.img"
n=0
fresh() {
    n=$((n + 1))
    copy_image "$1" "$SANDBOX/w$n.img"
    printf '%s\n' "$SANDBOX/w$n.img"
}

# random_bytes N: N bytes nobody would type. (BSD head refuses -c 0.)
random_bytes() {
    if [ "$1" -eq 0 ]; then return 0; fi
    head -c "$1" /dev/urandom
}

# unchanged NAME COPY BEFORE: the refusal left COPY byte for byte as BEFORE.
unchanged() {
    check "$1 left the image as it was" cmp -s "$2" "$3"
}

# A new file at every size boundary a 4 KiB block has, and a MiB.
for size in 0 1 4095 4096 4097 1048576; do
    img="$(fresh "$v5")"
    random_bytes "$size" >"$SANDBOX/src"
    fs.xfs "$img" write "/f$size" <"$SANDBOX/src" >"$SANDBOX/w.json" 2>"$SANDBOX/w.err"
    rc=$?
    check "write /f$size exits 0 ($(cat "$SANDBOX/w.err"))" test "$rc" -eq 0
    jq_check "write /f$size reports $size bytes, created, journalled" \
        ".path == \"/f$size\" and .bytes == $size and .result == \"created\" and .created == true and .journalled == true" \
        "$SANDBOX/w.json"
    fs.xfs "$img" read "/f$size" >"$SANDBOX/back" 2>/dev/null
    check "read /f$size is what was written" cmp -s "$SANDBOX/src" "$SANDBOX/back"
    fs.xfs "$img" ls "/f$size" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "ls /f$size is a 0644 file of $size bytes" \
        ".[0].type == \"file\" and .[0].size == $size and .[0].mode == \"0644\"" "$SANDBOX/ls.json"
    check "write /f$size leaves the log clean" \
        test "$(fs.xfs "$img" get dirty --text)" = false
done

# A new file in a subdirectory.
img="$(fresh "$v5")"
head -c 5000 /dev/urandom >"$SANDBOX/deep"
fs.xfs "$img" write /sub/nested/deep <"$SANDBOX/deep" >/dev/null 2>"$SANDBOX/e"
rc=$?
check "write /sub/nested/deep exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "read /sub/nested/deep is what was written" \
    cmp -s "$SANDBOX/deep" <(fs.xfs "$img" read /sub/nested/deep)
check "write /sub/nested/deep leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false

# An empty file given its contents.
img="$(fresh "$v5")"
head -c 6000 /dev/urandom >"$SANDBOX/fill"
fs.xfs "$img" write /empty <"$SANDBOX/fill" >"$SANDBOX/w.json" 2>/dev/null
jq_check "write /empty fills it" '.result == "filled" and .bytes == 6000 and .created == false' "$SANDBOX/w.json"
check "read /empty is what was written" cmp -s "$SANDBOX/fill" <(fs.xfs "$img" read /empty)
check "filling /empty leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false

# A same-length overwrite: in place, no journal, so the log stays clean --
# on v5 and on v4, where nothing about the metadata changes either.
for base in "$v5" "$v4"; do
    img="$(fresh "$base")"
    tag="$(basename "$base" .img)"
    head -c 262144 /dev/urandom >"$SANDBOX/same"
    fs.xfs "$img" write /medium.bin <"$SANDBOX/same" >"$SANDBOX/w.json" 2>"$SANDBOX/e"
    rc=$?
    check "$tag: write /medium.bin (same length) exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
    jq_check "$tag: the same-length write is an overwrite, not journalled" \
        '.result == "overwritten" and .journalled == false and .bytes == 262144' "$SANDBOX/w.json"
    check "$tag: read /medium.bin is what was written" cmp -s "$SANDBOX/same" <(fs.xfs "$img" read /medium.bin)
    check "$tag: an overwrite in place leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
    printf 'hello xfs!!\n' >"$SANDBOX/twelve"
    fs.xfs "$img" write /small.txt <"$SANDBOX/twelve" >/dev/null 2>&1
    check "$tag: the 12-byte /small.txt overwrites" cmp -s "$SANDBOX/twelve" <(fs.xfs "$img" read /small.txt)
done

# mkdir.
img="$(fresh "$v5")"
fs.xfs "$img" mkdir /d >"$SANDBOX/m.json" 2>"$SANDBOX/e"
rc=$?
check "mkdir /d exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
jq_check "mkdir reports the path and a numeric inode" '.path == "/d" and (.inode | type) == "number"' "$SANDBOX/m.json"
fs.xfs "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "ls / shows d as a 0755 directory" 'any(.[]; .name == "d" and .type == "dir" and .mode == "0755")' "$SANDBOX/ls.json"
fs.xfs "$img" ls /d >"$SANDBOX/lsd.json" 2>/dev/null
jq_check "ls /d is empty" '. == []' "$SANDBOX/lsd.json"
check "mkdir /d leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false

# Journalled writes chain on one copy, with no kernel between them: each
# finds the log the last one closed, and leaves it closed for the next.
fs.xfs "$img" mkdir /d/e >"$SANDBOX/m.json" 2>"$SANDBOX/e"
rc=$?
check "mkdir /d/e straight after mkdir /d exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "mkdir /d/e leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
head -c 5000 /dev/urandom >"$SANDBOX/chain1"
fs.xfs "$img" write /d/e/one <"$SANDBOX/chain1" >"$SANDBOX/w.json" 2>"$SANDBOX/e"
rc=$?
check "write /d/e/one straight after mkdir /d/e exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
jq_check "write /d/e/one is created and journalled" \
    '.result == "created" and .journalled == true and .bytes == 5000' "$SANDBOX/w.json"
check "write /d/e/one leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
head -c 4097 /dev/urandom >"$SANDBOX/chain2"
fs.xfs "$img" write /d/e/two <"$SANDBOX/chain2" >/dev/null 2>"$SANDBOX/e"
rc=$?
check "a second write, /d/e/two, on the same copy exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "write /d/e/two leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
head -c 3000 /dev/urandom >"$SANDBOX/chain3"
fs.xfs "$img" write /empty <"$SANDBOX/chain3" >"$SANDBOX/w.json" 2>"$SANDBOX/e"
rc=$?
check "filling /empty after the chain exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
jq_check "filling /empty after the chain is a fill" '.result == "filled"' "$SANDBOX/w.json"
check "the chain's last write leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
check "read /d/e/one is what was written" cmp -s "$SANDBOX/chain1" <(fs.xfs "$img" read /d/e/one)
check "read /d/e/two is what was written" cmp -s "$SANDBOX/chain2" <(fs.xfs "$img" read /d/e/two)
check "read /empty is what the chain wrote" cmp -s "$SANDBOX/chain3" <(fs.xfs "$img" read /empty)
fs.xfs "$img" ls /d/e >"$SANDBOX/lse.json" 2>/dev/null
jq_check "ls /d/e holds the two files the chain wrote" \
    '([.[].name] | sort) == ["one", "two"]' "$SANDBOX/lse.json"

# refused CODE PATTERN BASE PATH VERB [STDIN-BYTES]: on a fresh copy of
# BASE, VERB PATH fails with CODE, an error matching PATTERN, nothing on
# stdout, and the copy unchanged.
refused() {
    local code="$1" pattern="$2" base="$3" path="$4" verb="$5" bytes="${6:-}" img
    img="$(fresh "$base")"
    copy_image "$img" "$SANDBOX/before.img"
    if [ -n "$bytes" ]; then
        random_bytes "$bytes" | fs.xfs "$img" "$verb" "$path" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    else
        fs.xfs "$img" "$verb" "$path" </dev/null >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    fi
    rc=$?
    check "$verb $path on $(basename "$base") exits $code ($(cat "$SANDBOX/r.err"))" test "$rc" -eq "$code"
    check "$verb $path on $(basename "$base") prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "$verb $path on $(basename "$base") says why" \
        ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/r.err"
    unchanged "$verb $path on $(basename "$base")" "$img" "$SANDBOX/before.img"
    rm -f "$SANDBOX/before.img"
}

# Shapes the driver refuses, with its reason (exit 3).
refused 3 "grow the file" "$v5" /small.txt write 13
refused 3 "grow the file" "$v5" /medium.bin write 262145
refused 3 "not implemented.*shorter" "$v5" /medium.bin write 100
refused 3 "v4 filesystem is not supported" "$v4" /new write 10
refused 3 "v4 filesystem is not supported" "$v4" /d mkdir
refused 3 "v4 filesystem is not supported" "$v4" /empty write 10
# Mistakes (exit 1).
refused 1 "no such file" "$v5" /missing/f write 10
refused 1 "is a directory" "$v5" /sub write 10
refused 1 "already exists" "$v5" /sub mkdir
refused 1 "already exists" "$v5" /small.txt mkdir
refused 1 "not a directory" "$v5" /small.txt/x write 1

# Too large for any free run: refused with the driver's reason, and the
# file the write had created is removed again, so no half-made file is
# left. The volume is not byte-identical -- a create and an unlink are
# both records -- and the kernel oracle checks it is consistent.
img="$(fresh "$v5")"
head -c 100000000 /dev/zero | fs.xfs "$img" write /big >"$SANDBOX/b.out" 2>"$SANDBOX/b.err"
check "a write larger than any free run exits 3" test $? -eq 3
check "a write larger than any free run prints nothing on stdout" test ! -s "$SANDBOX/b.out"
jq_check "a write larger than any free run names the free run" '.code == 3 and (.error | test("free run"))' "$SANDBOX/b.err"
fs.xfs "$img" ls /big >/dev/null 2>"$SANDBOX/b2.err"
jq_check "the refused write left no /big behind" '.code == 1 and (.error | test("no such file"))' "$SANDBOX/b2.err"
check "the refused write's create and unlink leave the log clean" \
    test "$(fs.xfs "$img" get dirty --text)" = false

# A failing producer leaves the image as it was: stdin is read whole first.
img="$(fresh "$v5")"
copy_image "$img" "$SANDBOX/before.img"
{ head -c 10 /dev/urandom; exit 1; } | fs.xfs "$img" write /partial >/dev/null 2>&1
# The producer's status is its own; what matters is the volume. (A write
# of the ten bytes that did arrive is a legitimate outcome for a pipe
# whose writer died after writing, so only the refusal cases compare.)
check "a write from a pipe still leaves a readable volume" \
    test "$(fs.xfs "$img" get fs --text)" = xfs
rm -f "$SANDBOX/before.img"

# --offset reaches a partition for writing too.
disk="$SANDBOX/disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
printf 'in a partition\n' | fs.xfs --offset 1048576 "$disk" write /p.txt >/dev/null 2>"$SANDBOX/e"
rc=$?
check "write through --offset exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "read through --offset returns it" \
    test "$(fs.xfs --offset 1048576 "$disk" read /p.txt)" = "in a partition"
check "write through --offset leaves the log clean" \
    test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "the first MiB, outside the partition, is still zeros" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0

# rm and rmdir, chained on one copy: names the kernel made go, and each
# removal leaves the log clean for the next.
img="$(fresh "$v5")"

# removed VERB PATH DIR: VERB PATH exits 0, reports {"path", "removed":
# true}, DIR's listing no longer has the name, and the log is clean.
removed() {
    local verb="$1" path="$2" dir="$3" name="${2##*/}"
    fs.xfs "$img" "$verb" "$path" >"$SANDBOX/rm.json" 2>"$SANDBOX/rm.err"
    local rc=$?
    check "$verb $path exits 0 ($(cat "$SANDBOX/rm.err"))" test "$rc" -eq 0
    jq_check "$verb $path reports the path removed" \
        ".path == \"$path\" and .removed == true and (keys | length) == 2" "$SANDBOX/rm.json"
    fs.xfs "$img" ls "$dir" >"$SANDBOX/ls.json" 2>/dev/null
    jq_check "ls $dir no longer has $name after $verb" "all(.[]; .name != \"$name\")" "$SANDBOX/ls.json"
    check "$verb $path leaves the log clean" test "$(fs.xfs "$img" get dirty --text)" = false
}

free_before="$(fs.xfs "$img" get free_bytes --text)"
removed rm /large.bin /
free_after="$(fs.xfs "$img" get free_bytes --text)"
check "removing the 8 MiB /large.bin gave its blocks back ($free_before, then $free_after)" \
    test $((free_after - free_before)) -ge 8388608
removed rm /link-short /
check "removing the symlink left the file it named" test "$(fs.xfs "$img" read /small.txt)" = "hello world"
removed rm /sub/link-remote /sub
removed rm /manyfiles/entry-100.txt /manyfiles
removed rm /sub/nested/file.txt /sub/nested
removed rmdir /sub/nested /sub
fs.xfs "$img" rmdir --text /sub >"$SANDBOX/text.out" 2>/dev/null
check "rmdir --text says what it removed" test "$(cat "$SANDBOX/text.out")" = "removed /sub"
fs.xfs "$img" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the names beside the removed ones are still there" \
    '([.[].name] | sort) == (["empty", "fragmented.bin", "manyfiles", "medium.bin", "modes", "small.txt", "sparse.bin"] | sort)' \
    "$SANDBOX/ls.json"
fs.xfs "$img" ls /manyfiles >"$SANDBOX/ls.json" 2>/dev/null
jq_check "the other 199 entries of /manyfiles are still there" 'length == 199' "$SANDBOX/ls.json"
check "a file beside the removed ones still reads its bytes" test "$(fs.xfs "$img" read /modes/m0600)" = 0600

# Removals refused before the image is opened for writing: exit 1 for a
# mistake, with the reason Linux gives; exit 3 for a v4 volume and for a
# shape the driver does not free.
refused 1 "no such file" "$v5" /missing rm
refused 1 "no such file" "$v5" /missing rmdir
refused 1 "no such file" "$v5" /missing/f rm
refused 1 "is a directory" "$v5" /sub rm
refused 1 "is a directory" "$v5" / rm
refused 1 "is a directory" "$v5" /sub/. rm
refused 1 "not a directory" "$v5" /small.txt rmdir
refused 1 "not a directory" "$v5" /link-short rmdir
refused 1 "not a directory" "$v5" /small.txt/ rm
refused 1 "not a directory" "$v5" /small.txt/x rm
refused 1 "not empty" "$v5" /sub rmdir
refused 1 "not empty" "$v5" /sub/.. rmdir
refused 1 "invalid argument" "$v5" /sub/. rmdir
refused 1 "root directory" "$v5" / rmdir
refused 3 "v4 filesystem is not supported" "$v4" /small.txt rm
refused 3 "v4 filesystem is not supported" "$v4" /sub/nested rmdir
refused 3 "B.tree" "$v5" /fragmented.bin rm

# A log holding records nothing has applied is refused by every write
# verb: exit 1, the reason, nothing on stdout, nothing written. No verb
# here leaves such a log any more, so the volume is the kernel's own,
# shut down while mounted (`chore fixtures -- data` builds it).
dirty_base="$SHARE/xfsdirty.img"
if [ -f "$dirty_base" ]; then
    for verb in rm write mkdir; do
        img="$(fresh "$dirty_base")"
        copy_image "$img" "$SANDBOX/before.img"
        case "$verb" in
            rm) fs.xfs "$img" rm /d/renamed-1 </dev/null >"$SANDBOX/x.out" 2>"$SANDBOX/x.err" ;;
            write) printf x | fs.xfs "$img" write /new >"$SANDBOX/x.out" 2>"$SANDBOX/x.err" ;;
            mkdir) fs.xfs "$img" mkdir /new </dev/null >"$SANDBOX/x.out" 2>"$SANDBOX/x.err" ;;
        esac
        check "$verb on a volume whose log holds records exits 1" test $? -eq 1
        check "$verb on a dirty log prints nothing on stdout" test ! -s "$SANDBOX/x.out"
        jq_check "$verb on a dirty log says the log holds records" \
            '.code == 1 and (.error | test("log holds records"))' "$SANDBOX/x.err"
        unchanged "$verb on a dirty log" "$img" "$SANDBOX/before.img"
        rm -f "$SANDBOX/before.img" "$img"
    done
else
    fail ".vm-share/xfsdirty.img is missing; build it with \`chore fixtures -- data\`"
fi

img="$(fresh "$v5")"
fs.xfs "$img" rm >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "rm without a path is a usage error (exit 2)" test $? -eq 2
check "rm without a path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

# --offset reaches a partition for removing too, and nothing before it.
disk="$SANDBOX/rm-disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
fs.xfs --offset 1048576 "$disk" rm /small.txt >/dev/null 2>"$SANDBOX/e"
rc=$?
check "rm through --offset exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
fs.xfs --offset 1048576 "$disk" ls / >"$SANDBOX/ls.json" 2>/dev/null
jq_check "rm through --offset removed the name" 'all(.[]; .name != "small.txt")' "$SANDBOX/ls.json"
check "rm through --offset leaves the log clean" \
    test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "the first MiB, outside the partition, is still zeros after rm" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0

finish
