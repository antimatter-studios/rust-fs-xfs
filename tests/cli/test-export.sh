# fs.xfs export, as installed, on the kernel-made images, against the
# manifest the kernel's own XFS driver wrote for each: a file copied out
# has the SHA-256 the kernel computed, an empty one, one that crosses the
# transfer's chunk boundaries (8 MiB), a sparse one and one whose extents are
# scattered; a tree copied out has every file's kernel digest, every
# symlink's kernel target and (with --preserve=times) every modification
# time the kernel recorded. The image is not changed by any of it.
#
# Then the refusals: an existing DEST without --replace, a directory
# without --recursive, a missing path, and a copy whose /small.txt inode
# fails its checksum, each leaving no destination file and nothing on
# stdout. A 32-directory-deep tree and a file of 3 MiB plus a byte, made
# and digested by the kernel, are tests/cli_export_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img xfscli-v5.manifest xfscli-v4.manifest xfscli-v5.corrupt

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

# The modification time of a local path, in seconds.
mtime() {
    stat -c %Y "$1" 2>/dev/null || stat -f %m "$1"
}

for name in v5 v4; do
    img="$SHARE/xfscli-$name.img"
    manifest="$SHARE/xfscli-$name.manifest"
    before="$(sha256 <"$img")"

    # One file at a time, to a local file: the kernel's digest, the report's
    # byte count and digest, and no staging name left beside it.
    for path in /empty /small.txt /medium.bin /large.bin /sparse.bin /fragmented.bin; do
        want="$(awk -F'\t' -v p="$path" '$1 == p { print $6 }' "$manifest")"
        size="$(awk -F'\t' -v p="$path" '$1 == p { print $3 }' "$manifest")"
        dest="$SANDBOX/$name$(basename "$path")"
        fs.xfs "$img" export "$path" "$dest" >"$SANDBOX/e.json" 2>"$SANDBOX/e.err"
        rc=$?
        check "$name: export $path exits 0 ($(cat "$SANDBOX/e.err"))" test "$rc" -eq 0
        check "$name: export $path has the kernel's digest $want" test "$(sha256 <"$dest")" = "$want"
        jq_check "$name: export $path reports $size bytes and the digest" \
            ".bytes == $size and .sha256 == \"$want\" and .replaced == false" "$SANDBOX/e.json"
        check "$name: export $path leaves no .partial beside it" \
            test -z "$(find "$SANDBOX" -maxdepth 1 -name ".*.partial" -o -maxdepth 1 -name "*.partial")"
    done

    # To stdout: the bytes and nothing else.
    check "$name: export to - is the kernel's digest of /large.bin" \
        test "$(fs.xfs "$img" export /large.bin - 2>/dev/null | sha256)" = \
        "$(awk -F'\t' '$1 == "/large.bin" { print $6 }' "$manifest")"

    # An existing DEST is refused unless --replace says so, and the refusal
    # leaves it as it was.
    printf 'keep me' >"$SANDBOX/keep"
    fs.xfs "$img" export /small.txt "$SANDBOX/keep" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    check "$name: export onto an existing file exits 1" test $? -eq 1
    check "$name: export onto an existing file prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    jq_check "$name: export onto an existing file says to give --replace" \
        '.code == 1 and (.error | test("--replace"))' "$SANDBOX/r.err"
    check "$name: a refused export leaves the file as it was" test "$(cat "$SANDBOX/keep")" = "keep me"
    fs.xfs "$img" export --replace /small.txt "$SANDBOX/keep" >"$SANDBOX/r.json" 2>/dev/null
    check "$name: export --replace exits 0" test $? -eq 0
    check "$name: export --replace puts the file there" \
        test "$(sha256 <"$SANDBOX/keep")" = "$(awk -F'\t' '$1 == "/small.txt" { print $6 }' "$manifest")"
    jq_check "$name: export --replace says it replaced" '.replaced == true' "$SANDBOX/r.json"

    # --verify reads the staged copy back before publishing it.
    fs.xfs "$img" export --verify /medium.bin "$SANDBOX/$name-verified" >"$SANDBOX/v.json" 2>/dev/null
    check "$name: export --verify exits 0" test $? -eq 0
    jq_check "$name: export --verify reports verified" '.verified == true' "$SANDBOX/v.json"

    # Refusals: status 1, a structured error, nothing on stdout, no DEST.
    for case in "/sub:is a directory" "/link-short:is a symlink to small.txt" "/sub/nested/file.txt/x:not a directory"; do
        path="${case%%:*}"
        why="${case#*:}"
        rm -f "$SANDBOX/none"
        fs.xfs "$img" export "$path" "$SANDBOX/none" >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
        check "$name: export $path exits 1" test $? -eq 1
        check "$name: export $path prints nothing on stdout" test ! -s "$SANDBOX/r.out"
        jq_check "$name: export $path says '$why'" ".code == 1 and (.error | test(\"$why\"))" "$SANDBOX/r.err"
        check "$name: export $path creates no DEST" test ! -e "$SANDBOX/none"
    done
    fs.xfs "$img" export /missing "$SANDBOX/none" >"$SANDBOX/nf.out" 2>"$SANDBOX/nf.err"
    check "$name: export of a missing path exits 1" test $? -eq 1
    jq_check "$name: export of a missing path is the not_found kind" \
        '.code == 1 and .kind == "not_found"' "$SANDBOX/nf.err"
    check "$name: export of a missing path creates no DEST" test ! -e "$SANDBOX/none"

    # A tree: every item reported, every file the kernel's digest, every
    # symlink the kernel's target, every modification time the kernel's.
    tree="$SANDBOX/$name-tree"
    mkdir "$tree"
    fs.xfs "$img" export --recursive --preserve=links,times / "$tree" >"$SANDBOX/t.json" 2>"$SANDBOX/t.err"
    rc=$?
    check "$name: export --recursive / exits 0 ($(head -c 300 "$SANDBOX/t.err"))" test "$rc" -eq 0
    entries="$(wc -l <"$manifest" | tr -d ' ')"
    jq_check "$name: the tree report has $entries entries and the root, all completed" \
        ".failed == 0 and .refused == 0 and .skipped == 0 and .completed == $((entries + 1))" "$SANDBOX/t.json"
    nfiles=0
    nlinks=0
    while IFS=$'\t' read -r path type size mode mtime sum; do
        local_path="$tree$path"
        case "$type" in
        file)
            nfiles=$((nfiles + 1))
            check "$name: tree $path has the kernel's digest" test "$(sha256 <"$local_path")" = "$sum"
            check "$name: tree $path has the kernel's modification time" test "$(mtime "$local_path")" = "$mtime"
            ;;
        dir) check "$name: tree $path is a directory" test -d "$local_path" ;;
        symlink)
            nlinks=$((nlinks + 1))
            check "$name: tree $path links to the kernel's target" test "$(readlink "$local_path")" = "$sum"
            ;;
        esac
    done <"$manifest"
    check "$name: every file of the tree was checked ($nfiles)" test "$nfiles" -gt 200
    check "$name: every symlink of the tree was checked ($nlinks)" test "$nlinks" -ge 2

    # A tree below a directory, to a destination that is made for it.
    sub="$SANDBOX/$name-sub"
    fs.xfs "$img" export --recursive /sub "$sub" >"$SANDBOX/s.json" 2>/dev/null
    check "$name: a symlink without --preserve=links is refused, so the tree exits nonzero" test $? -ne 0
    jq_check "$name: the refused item is the symlink and the rest completed" \
        '.refused == 1 and .failed == 0 and ([.items[] | select(.status == "refused") | .path] == ["/sub/link-remote"])' \
        "$SANDBOX/s.json"
    check "$name: the file under it was still copied" \
        test "$(sha256 <"$sub/nested/file.txt")" = "$(awk -F'\t' '$1 == "/sub/nested/file.txt" { print $6 }' "$manifest")"

    # Reading the image never changes it.
    check "$name: no export changed the image" test "$(sha256 <"$img")" = "$before"
done

# A copy whose /small.txt inode fails its CRC (v5): the item fails, with a
# structured error, nothing on stdout and no destination file.
v5="$SHARE/xfscli-v5.img"
at="$(awk -F'\t' '$1 == "inode-crc" { print $2 }' "$SHARE/xfscli-v5.corrupt")"
check "the .corrupt file locates /small.txt's inode" test -n "$at"
copy_image "$v5" "$SANDBOX/bad.img"
poke "$SANDBOX/bad.img" "$at"
fs.xfs "$SANDBOX/bad.img" export /small.txt "$SANDBOX/bad.out" >"$SANDBOX/b.out" 2>"$SANDBOX/b.err"
check "export of a file whose inode fails its checksum exits nonzero" test $? -ne 0
check "export of a file whose inode fails its checksum prints nothing on stdout" test ! -s "$SANDBOX/b.out"
jq_check "export of a file whose inode fails its checksum is a structured error" '.code >= 1' "$SANDBOX/b.err"
check "export of a file whose inode fails its checksum creates no DEST" test ! -e "$SANDBOX/bad.out"
mkdir "$SANDBOX/bad-tree"
fs.xfs "$SANDBOX/bad.img" export --recursive / "$SANDBOX/bad-tree" >"$SANDBOX/bt.json" 2>/dev/null
check "a tree with an unreadable file exits nonzero" test $? -ne 0
check "a tree with an unreadable file creates no small.txt" test ! -e "$SANDBOX/bad-tree/small.txt"

# The help carries an example, and says where DEST goes.
fs.xfs "$v5" export --help >"$SANDBOX/help" 2>&1
check "export --help carries an example" grep -q 'Examples:' "$SANDBOX/help"
check "export --help shows --recursive and --preserve" \
    grep -q -e '--recursive' -e '--preserve' "$SANDBOX/help"

finish
