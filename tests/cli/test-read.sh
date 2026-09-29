# fs.xfs ls and read on the kernel-made images, against the manifest the
# kernel's own XFS driver wrote for each: every directory's listing has the
# names, types, sizes, modes, mtimes and link targets the kernel recorded,
# and every file reads back with the SHA-256 the kernel computed -- an
# extent file, an empty one, a sparse one, one whose block map is a B+tree,
# and two hundred in a directory past short form. Then the failures: a
# directory, a symlink, a missing path, and a copy with an inode that
# fails its checksum or an AG header with a bad magic, each a structured
# error with nothing on stdout.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img xfscli-v5.manifest xfscli-v4.manifest xfscli-v5.corrupt

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

for name in v5 v4; do
    img="$SHARE/xfscli-$name.img"
    manifest="$SHARE/xfscli-$name.manifest"

    # Every directory the kernel recorded, and the root.
    dirs="$(printf '/\n'; awk -F'\t' '$2 == "dir" { print $1 }' "$manifest")"
    ndirs=0
    while IFS= read -r dir; do
        ndirs=$((ndirs + 1))
        # What the kernel recorded for this directory's children: name,
        # type, size (not for a directory, whose size is its own layout),
        # mode, mtime, and a symlink's target.
        awk -F'\t' -v dir="$dir" '{
            parent = $1; sub(/\/[^\/]*$/, "", parent); if (parent == "") parent = "/"
            if (parent != dir) next
            base = $1; sub(/^.*\//, "", base)
            printf "%s\t%s\t%s\t%s\t%s\t%s\n", base, $2, ($2 == "dir" ? "-" : $3), $4, $5, ($2 == "symlink" ? $6 : "-")
        }' "$manifest" | sort >"$SANDBOX/want"
        if ! fs.xfs "$img" ls "$dir" >"$SANDBOX/ls.json" 2>"$SANDBOX/ls.err"; then
            fail "$name: ls $dir failed: $(cat "$SANDBOX/ls.err")"
            continue
        fi
        jq -r '.[] | [.name, .type, (if .type == "dir" then "-" else (.size|tostring) end), .mode, (.mtime|tostring), (.target // "-")] | @tsv' \
            "$SANDBOX/ls.json" | sort >"$SANDBOX/got"
        if cmp -s "$SANDBOX/want" "$SANDBOX/got"; then
            ok
        else
            fail "$name: ls $dir differs from the kernel's manifest (< kernel, > fs.xfs):
$(diff "$SANDBOX/want" "$SANDBOX/got" | head -20)"
        fi
    done <<<"$dirs"
    check "$name: every directory was listed ($ndirs)" test "$ndirs" -ge 5

    jq_check "$name: every ls field has its JSON type" \
        'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number")' \
        "$SANDBOX/ls.json"

    # Every file's bytes, against the kernel's SHA-256.
    nfiles=0
    while IFS=$'\t' read -r path type size mode mtime sum; do
        [ "$type" = file ] || continue
        nfiles=$((nfiles + 1))
        got="$(fs.xfs "$img" read "$path" 2>"$SANDBOX/read.err" | sha256)"
        check "$name: read $path hashes as the kernel's $sum (got $got; $(cat "$SANDBOX/read.err"))" \
            test "$got" = "$sum"
    done <"$manifest"
    check "$name: every file was read ($nfiles)" test "$nfiles" -gt 200

    # One file by path lists as itself; --text is one line per entry.
    fs.xfs "$img" ls /small.txt >"$SANDBOX/one.json" 2>/dev/null
    jq_check "$name: ls of a file is that one entry" 'length == 1 and .[0].name == "small.txt" and .[0].size == 12' "$SANDBOX/one.json"
    check "$name: ls --text / is one line per entry" \
        test "$(fs.xfs "$img" ls --text / | wc -l | tr -d ' ')" -eq "$(fs.xfs "$img" ls / | jq length)"
    check "$name: ls --text shows a symlink's target" \
        grep -q 'link-short -> small.txt' <<<"$(fs.xfs "$img" ls --text /)"

    # -o writes the file whole, and nothing on stdout.
    fs.xfs "$img" read /medium.bin -o "$SANDBOX/medium.out" >"$SANDBOX/o.stdout" 2>/dev/null
    check "$name: read -o exits 0" test $? -eq 0
    check "$name: read -o prints nothing on stdout" test ! -s "$SANDBOX/o.stdout"
    check "$name: read -o writes the file's bytes" \
        test "$(sha256 <"$SANDBOX/medium.out")" = "$(awk -F'\t' '$1 == "/medium.bin" { print $6 }' "$manifest")"
    check "$name: read -o leaves no .partial behind" test ! -e "$SANDBOX/medium.out.partial"

    # Refusals: status 1, a structured error naming why, nothing on stdout.
    for case in "/sub:is a directory" "/link-short:is a symlink to small.txt" "/missing:no such file" "/sub/nested/file.txt/x:not a directory"; do
        path="${case%%:*}"
        why="${case#*:}"
        fs.xfs "$img" read "$path" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
        check "$name: read $path exits 1" test $? -eq 1
        check "$name: read $path prints nothing on stdout" test ! -s "$SANDBOX/r.out"
        jq_check "$name: read $path says '$why'" ".code == 1 and (.error | test(\"$why\"))" "$SANDBOX/r.err"
    done
    fs.xfs "$img" ls /missing >"$SANDBOX/lm.out" 2>"$SANDBOX/lm.err"
    check "$name: ls of a missing path exits 1" test $? -eq 1
    check "$name: ls of a missing path prints nothing on stdout" test ! -s "$SANDBOX/lm.out"
    jq_check "$name: ls of a missing path says so" '.code == 1 and (.error | test("no such file"))' "$SANDBOX/lm.err"
done

# A copy whose /small.txt inode fails its CRC (v5): every verb that reads
# that inode fails, with a structured error and not one byte on stdout.
v5="$SHARE/xfscli-v5.img"
at="$(awk -F'\t' '$1 == "inode-crc" { print $2 }' "$SHARE/xfscli-v5.corrupt")"
check "the .corrupt file locates /small.txt's inode" test -n "$at"
copy_image "$v5" "$SANDBOX/badcrc.img"
poke "$SANDBOX/badcrc.img" "$at"
for verb in "read /small.txt" "ls /small.txt" "ls /"; do
    # shellcheck disable=SC2086  # the words are the point
    fs.xfs "$SANDBOX/badcrc.img" $verb >"$SANDBOX/c.out" 2>"$SANDBOX/c.err"
    check "a bad inode CRC: $verb exits 1" test $? -eq 1
    check "a bad inode CRC: $verb prints nothing on stdout" test ! -s "$SANDBOX/c.out"
    jq_check "a bad inode CRC: $verb names the checksum" '.code == 1 and (.error | test("CRC"))' "$SANDBOX/c.err"
done
fs.xfs "$SANDBOX/badcrc.img" read /small.txt -o "$SANDBOX/never" >/dev/null 2>&1
check "a bad inode CRC: read -o creates no file" test ! -e "$SANDBOX/never"
check "a bad inode CRC: read -o leaves no .partial" test ! -e "$SANDBOX/never.partial"
check "a bad inode CRC: another file still reads" \
    test "$(fs.xfs "$SANDBOX/badcrc.img" read /sub/nested/file.txt 2>&1)" = nested

# An AG header with a bad magic: the mount itself is refused, so every
# verb fails the same way.
at="$(awk -F'\t' '$1 == "agi-magic" { print $2 }' "$SHARE/xfscli-v5.corrupt")"
copy_image "$v5" "$SANDBOX/badagi.img"
poke "$SANDBOX/badagi.img" "$at"
for verb in "ls /" "read /small.txt" "get" "info label"; do
    # shellcheck disable=SC2086  # the words are the point
    fs.xfs "$SANDBOX/badagi.img" $verb >"$SANDBOX/a.out" 2>"$SANDBOX/a.err"
    check "a bad AGI magic: $verb exits 1" test $? -eq 1
    check "a bad AGI magic: $verb prints nothing on stdout" test ! -s "$SANDBOX/a.out"
    jq_check "a bad AGI magic: $verb names the AGI" '.code == 1 and (.error | test("AGI"))' "$SANDBOX/a.err"
done

finish
