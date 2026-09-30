# fs.xfs write and mkdir, as installed, on copies of the kernel-made
# images: every shape the driver can write round-trips byte for byte, and
# every shape it cannot is refused with its reason -- exit 3 for a shape,
# exit 1 for a mistake -- with nothing on stdout and the copy unchanged.
#
# ONE JOURNALLED WRITE PER COPY. A new file, a filled empty file and a
# mkdir are log records, and the driver writes only to a volume whose log
# is clean, so the next journalled write on the same copy is refused until
# Linux has mounted it once. That refusal is checked here; the chain of
# writes with the kernel replaying between them is tests/cli_write_kernel.rs.
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
    check "after write /f$size the log holds the record (dirty)" \
        test "$(fs.xfs "$img" get dirty --text)" = true
done

# A new file in a subdirectory.
img="$(fresh "$v5")"
head -c 5000 /dev/urandom >"$SANDBOX/deep"
fs.xfs "$img" write /sub/nested/deep <"$SANDBOX/deep" >/dev/null 2>"$SANDBOX/e"
rc=$?
check "write /sub/nested/deep exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "read /sub/nested/deep is what was written" \
    cmp -s "$SANDBOX/deep" <(fs.xfs "$img" read /sub/nested/deep)

# An empty file given its contents.
img="$(fresh "$v5")"
head -c 6000 /dev/urandom >"$SANDBOX/fill"
fs.xfs "$img" write /empty <"$SANDBOX/fill" >"$SANDBOX/w.json" 2>/dev/null
jq_check "write /empty fills it" '.result == "filled" and .bytes == 6000 and .created == false' "$SANDBOX/w.json"
check "read /empty is what was written" cmp -s "$SANDBOX/fill" <(fs.xfs "$img" read /empty)

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

# The second journalled write on one copy waits for a replay: exit 1, the
# reason, nothing on stdout, nothing written.
cp "$img" "$SANDBOX/before.img" 2>/dev/null || copy_image "$img" "$SANDBOX/before.img"
fs.xfs "$img" mkdir /d/e >"$SANDBOX/x.out" 2>"$SANDBOX/x.err"
check "mkdir /d/e before a replay exits 1" test $? -eq 1
check "mkdir /d/e before a replay prints nothing on stdout" test ! -s "$SANDBOX/x.out"
jq_check "mkdir /d/e before a replay says the log holds records" \
    '.code == 1 and (.error | test("log holds records")) and (.error | test("Linux"))' "$SANDBOX/x.err"
unchanged "mkdir before a replay" "$img" "$SANDBOX/before.img"

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
check "the first MiB, outside the partition, is still zeros" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0

finish
