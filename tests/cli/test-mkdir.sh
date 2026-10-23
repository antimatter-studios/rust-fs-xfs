# mkdir --parents: preflight the whole path, create only missing directories,
# and release the one writable mount clean. The kernel grades the result in
# tests/cli_mkdir_kernel.rs; these checks exercise the installed tool.
source "$(dirname "$0")/lib.sh"
need_fixture xfscli-v5.img xfscli-v4.img
img="$SANDBOX/parents.img"
copy_image "$SHARE/xfscli-v5.img" "$img"

fs.xfs "$img" mkdir --parents /a/b/c >"$SANDBOX/report" 2>"$SANDBOX/error"
rc=$?
check "a three-level chain succeeds ($(cat "$SANDBOX/error"))" test "$rc" -eq 0
jq_check "the report lists only directories made, in order" \
    '. == {path: "/a/b/c", created: ["/a", "/a/b", "/a/b/c"]}' "$SANDBOX/report"
check "the chain is released clean" test "$(fs.xfs "$img" get dirty --text)" = false
for dir in /a /a/b /a/b/c; do
    fs.xfs "$img" stat "$dir" >"$SANDBOX/stat" 2>/dev/null
    jq_check "$dir is a 0755 directory" '.type == "dir" and .mode == "0755"' "$SANDBOX/stat"
done

# Existing directories are a read-only success, including root. The short
# option, relative paths, repeated slashes and dot components work too.
for dir in /a/b/c /sub/nested /; do
    copy_image "$img" "$SANDBOX/before"
    fs.xfs "$img" mkdir -p "$dir" >"$SANDBOX/report" 2>"$SANDBOX/error"
    check "existing $dir succeeds" test $? -eq 0
    jq_check "existing $dir creates nothing" '.created == []' "$SANDBOX/report"
    check "existing $dir leaves every byte unchanged" cmp -s "$img" "$SANDBOX/before"
done
fs.xfs "$img" mkdir -p a//b/./d/ >"$SANDBOX/report" 2>"$SANDBOX/error"
check "an existing prefix succeeds" test $? -eq 0
jq_check "only the missing suffix is reported" \
    '.path == "a//b/./d/" and .created == ["/a/b/d"]' "$SANDBOX/report"
fs.xfs "$img" mkdir -p /a/b/d --text >"$SANDBOX/text" 2>/dev/null
check "text reports an existing chain" test "$(cat "$SANDBOX/text")" = 'already exists /a/b/d'
fs.xfs "$img" mkdir -p /a/b/e --text >"$SANDBOX/text" 2>/dev/null
check "text names the directory created" test "$(cat "$SANDBOX/text")" = 'created /a/b/e'
fs.xfs "$img" mkdir -p /walk/child/../child/leaf >"$SANDBOX/report" 2>"$SANDBOX/error"
check "a planned directory can be visited again after dot-dot" test $? -eq 0
jq_check "planned directories are created only once" \
    '.created == ["/walk", "/walk/child", "/walk/child/leaf"]' "$SANDBOX/report"

# A later obstruction or invalid name must not leave an earlier missing
# component behind. Symlinks are refused as non-directories, never followed.
long="$(printf '%0256d' 0)"
for dir in /small.txt /small.txt/child /link-short/child \
    /new/../small.txt/child "/new/$long"; do
    copy_image "$img" "$SANDBOX/before"
    fs.xfs "$img" mkdir -p "$dir" >"$SANDBOX/report" 2>"$SANDBOX/error"
    rc=$?
    check "mkdir -p $dir is refused" test "$rc" -ne 0
    check "a refusal prints no success report" test ! -s "$SANDBOX/report"
    jq_check "a refusal uses the shared JSON error" \
        ".code == $rc and (.error | length) > 0" "$SANDBOX/error"
    check "a refusal leaves every byte unchanged" cmp -s "$img" "$SANDBOX/before"
done

# Plain mkdir retains its old report and refuses existing or missing parents.
fs.xfs "$img" mkdir /plain >"$SANDBOX/report" 2>/dev/null
jq_check "plain mkdir still reports an inode" '.path == "/plain" and (.inode | type) == "number" and (has("created") | not)' "$SANDBOX/report"
for dir in /plain /absent/child; do
    copy_image "$img" "$SANDBOX/before"
    fs.xfs "$img" mkdir "$dir" >"$SANDBOX/report" 2>"$SANDBOX/error"
    check "plain mkdir $dir is refused by the volume's state (exit 4)" test $? -eq 4
    check "plain mkdir refusal changes nothing" cmp -s "$img" "$SANDBOX/before"
done
copy_image "$SHARE/xfscli-v4.img" "$SANDBOX/v4.img"
copy_image "$SANDBOX/v4.img" "$SANDBOX/before"
fs.xfs "$SANDBOX/v4.img" mkdir -p /new/child >"$SANDBOX/report" 2>"$SANDBOX/error"
check "creating on v4 is refused" test $? -eq 3
check "v4 refusal changes nothing" cmp -s "$SANDBOX/v4.img" "$SANDBOX/before"

# A read-only replay can resolve paths, but creation on its dirty log is refused.
dirty="$SHARE/xfsdirty.img"
if [ -f "$dirty" ]; then
    copy_image "$dirty" "$SANDBOX/dirty.img"
    copy_image "$SANDBOX/dirty.img" "$SANDBOX/before"
    fs.xfs "$SANDBOX/dirty.img" mkdir -p /parents/a/b >"$SANDBOX/report" 2>"$SANDBOX/error"
    check "parents on a dirty log fails" test $? -eq 1
    check "dirty-log refusal prints no success" test ! -s "$SANDBOX/report"
    jq_check "dirty-log refusal explains the log" \
        '.code == 1 and (.error | test("log holds records"))' "$SANDBOX/error"
    check "dirty-log refusal changes nothing" cmp -s "$SANDBOX/dirty.img" "$SANDBOX/before"
else
    fail ".vm-share/xfsdirty.img is missing; build it with \`chore fixtures -- data\`"
fi

# Offset remains the start of the filesystem, and bytes before it stay intact.
disk="$SANDBOX/disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$SHARE/xfscli-v5.img" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
fs.xfs --offset 1048576 "$disk" mkdir -p /part/a/b >"$SANDBOX/report" 2>"$SANDBOX/error"
check "parents through --offset succeeds" test $? -eq 0
jq_check "offset chain reports volume paths" '.created == ["/part", "/part/a", "/part/a/b"]' "$SANDBOX/report"
check "offset chain leaves the log clean" test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "the first MiB stays zero" \
    test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0
finish
