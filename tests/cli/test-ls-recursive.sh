# Recursive listing on the existing kernel-made nested tree, v4 and v5.
source "$(dirname "$0")/lib.sh"
need_fixture xfscli-v5.img xfscli-v4.img

for version in v5 v4; do
    img="$SHARE/xfscli-$version.img"
    copy_image "$img" "$SANDBOX/before.img"
    fs.xfs "$img" ls --recursive /sub >"$SANDBOX/tree.json" 2>"$SANDBOX/tree.err"
    check "$version: recursion exits 0" test $? -eq 0
    check "$version: success has no error" test ! -s "$SANDBOX/tree.err"
    jq_check "$version: byte-sorted depth-first paths" \
        '[.[].path]==["/sub/link-remote","/sub/nested","/sub/nested/file.txt"] and [.[].depth]==[1,1,2]' "$SANDBOX/tree.json"
    jq_check "$version: entries retain the plain ls fields" \
        'all(.[]; (.name|type)=="string" and (.type|type)=="string" and (.size|type)=="number" and (.mode|test("^[0-7]{4}$")) and (.mtime|type)=="number" and (.inode|type)=="number" and (has("revisited")|not))' "$SANDBOX/tree.json"
    fs.xfs "$img" ls -R sub//./ >"$SANDBOX/again.json" 2>/dev/null
    check "$version: relative paths and separators resolve" test $? -eq 0
    check "$version: two runs print identical bytes" cmp -s "$SANDBOX/tree.json" "$SANDBOX/again.json"
    fs.xfs "$img" ls -R --max-depth 2 --limit 3 /sub >"$SANDBOX/fit.json" 2>/dev/null
    check "$version: exact bounds succeed" test $? -eq 0
    check "$version: exact bounds retain every entry" cmp -s "$SANDBOX/tree.json" "$SANDBOX/fit.json"
    for spec in 'max-depth 1 2 /sub/nested/file.txt' 'limit 1 1 /sub/nested' 'max-depth 0 0 /sub/link-remote' 'limit 0 0 /sub/link-remote'; do
        read -r bound value count stopped <<<"$spec"
        fs.xfs "$img" ls -R "--$bound" "$value" /sub >"$SANDBOX/partial.json" 2>"$SANDBOX/partial.err"
        check "$version: $bound $value exits 1" test $? -eq 1
        check "$version: $bound $value retains the exact prefix" \
            test "$(jq -c . "$SANDBOX/partial.json")" = "$(jq -c ".[0:$count]" "$SANDBOX/tree.json")"
        jq_check "$version: $bound $value names the bound and path" \
            ".code==1 and (.error|contains(\"--$bound $value\")) and (.error|contains(\"$stopped\"))" "$SANDBOX/partial.err"
    done
    fs.xfs "$img" ls -R --max-depth 0 --limit 1 /small.txt >"$SANDBOX/file.json" 2>/dev/null
    check "$version: starting file fits depth zero" test $? -eq 0
    jq_check "$version: starting file is one entry at depth zero" \
        'length==1 and .[0].path=="/small.txt" and .[0].depth==0 and .[0].size==12' "$SANDBOX/file.json"
    fs.xfs "$img" ls -R /link-short >"$SANDBOX/link.json" 2>/dev/null
    check "$version: starting symlink exits 0" test $? -eq 0
    jq_check "$version: starting symlink is never followed" \
        'length==1 and .[0].depth==0 and .[0].type=="symlink" and .[0].target=="small.txt"' "$SANDBOX/link.json"
    fs.xfs "$img" ls -R --text /sub >"$SANDBOX/tree.txt" 2>/dev/null
    check "$version: text recursion exits 0" test $? -eq 0
    check "$version: text uses full paths" grep -q ' /sub/nested/file.txt$' "$SANDBOX/tree.txt"
    fs.xfs --text "$img" ls -R --json --limit 1 /sub >"$SANDBOX/format.json" 2>"$SANDBOX/format.err"
    check "$version: partial JSON exits 1" test $? -eq 1
    jq_check "$version: partial output respects the last format switch" 'length==1' "$SANDBOX/format.json"
    fs.xfs --json "$img" ls -R --text --limit 1 /sub >"$SANDBOX/format.txt" 2>"$SANDBOX/format.err"
    check "$version: partial text exits 1" test $? -eq 1
    check "$version: partial text names the full path" grep -q ' /sub/link-remote -> ' "$SANDBOX/format.txt"
    check "$version: partial text error names the bound" grep -q -- '--limit 1' "$SANDBOX/format.err"
    fs.xfs "$img" ls /sub >"$SANDBOX/plain.json" 2>/dev/null
    check "$version: plain ls exits 0" test $? -eq 0
    jq_check "$version: plain ls keeps its shape" \
        'length==2 and all(.[]; (has("path") or has("depth") or has("revisited"))|not)' "$SANDBOX/plain.json"
    fs.xfs "$img" ls -R /missing >"$SANDBOX/missing.out" 2>"$SANDBOX/missing.err"
    check "$version: missing start exits 1" test $? -eq 1
    check "$version: missing start prints no report" test ! -s "$SANDBOX/missing.out"
    jq_check "$version: missing start has a structured error" '.code==1' "$SANDBOX/missing.err"
    check "$version: listing leaves the image unchanged" cmp -s "$img" "$SANDBOX/before.img"
done

for args in '--max-depth 2' '--limit 2' '-R --limit nope' '-R --max-depth -1'; do
    # shellcheck disable=SC2086 # The words form the command line under test.
    fs.xfs "$img" ls $args / >"$SANDBOX/usage.out" 2>"$SANDBOX/usage.err"
    check "ls $args exits 2" test $? -eq 2
    check "ls $args prints no report" test ! -s "$SANDBOX/usage.out"
    jq_check "ls $args has a structured usage error" '.code==2' "$SANDBOX/usage.err"
done
{ head -c 1048576 /dev/zero; cat "$img"; } >"$SANDBOX/embedded.img"
fs.xfs --offset 1048576 "$SANDBOX/embedded.img" ls -R /sub >"$SANDBOX/offset.json" 2>/dev/null
check "--offset still locates the filesystem" test $? -eq 0
check "--offset retains recursive metadata" cmp -s "$SANDBOX/tree.json" "$SANDBOX/offset.json"
check "refused commands leave the image unchanged" cmp -s "$img" "$SANDBOX/before.img"
finish
