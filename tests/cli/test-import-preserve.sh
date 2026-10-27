# fs.xfs import --preserve=times,links, as installed: a tree with a
# three-name hard link, a relative and an absolute symbolic link and set
# times comes back with one inode of link count 3, the link targets
# byte-identical and the modification and access times equal. What the
# kernel and xfs_repair make of it is tests/cli_import_kernel.rs. Without
# the flag a link is refused per item, never dropped.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img

v5="$SHARE/xfscli-v5.img"
tree="$SANDBOX/tree"
mkdir -p "$tree/d"
echo data >"$tree/f1"
ln "$tree/f1" "$tree/d/f2"
ln "$tree/f1" "$tree/f3"
ln -s f1 "$tree/rel"
ln -s /abs/target "$tree/abs"
touch -t 202001020304.05 "$tree/f1" "$tree/d"

copy_image "$v5" "$SANDBOX/p.img"
img="$SANDBOX/p.img"
fs.xfs "$img" import --recursive --preserve=times,links "$tree" /t >"$SANDBOX/r.json" 2>"$SANDBOX/r.err"
rc=$?
check "import --preserve=times,links exits 0 ($(head -c 300 "$SANDBOX/r.err"))" test "$rc" -eq 0
jq_check "every item completed" '.failed == 0 and .refused == 0 and .skipped == 0 and .completed == 7' "$SANDBOX/r.json"

for p in /t/f1 /t/d/f2 /t/f3; do
    fs.xfs "$img" stat "$p" >"$SANDBOX/s.json"
    jq_check "$p has link count 3" '.nlink == 3 and .type == "file"' "$SANDBOX/s.json"
    echo "$(jq -r .inode "$SANDBOX/s.json")" >>"$SANDBOX/inodes"
done
check "the three names are one inode" test "$(sort -u "$SANDBOX/inodes" | wc -l | tr -d ' ')" -eq 1
fs.xfs "$img" ls /t >"$SANDBOX/ls.json"
jq_check "the relative link has its target" '[.[] | select(.name == "rel" and .type == "symlink" and .target == "f1")] | length == 1' "$SANDBOX/ls.json"
jq_check "the absolute link has its target" '[.[] | select(.name == "abs" and .type == "symlink" and .target == "/abs/target")] | length == 1' "$SANDBOX/ls.json"
want="$(stat -c %Y "$tree/f1" 2>/dev/null || stat -f %m "$tree/f1")"
for p in /t/f1 /t/d; do
    fs.xfs "$img" stat "$p" >"$SANDBOX/s.json"
    jq_check "$p has the modification and access times sent ($want)" ".mtime == $want and .atime == $want" "$SANDBOX/s.json"
done
check "the file reads back" test "$(fs.xfs "$img" read /t/d/f2)" = data
fs.xfs "$img" get dirty --text >"$SANDBOX/dirty"
check "the volume is left clean" test "$(cat "$SANDBOX/dirty")" = false

# Without --preserve=links the links are refused, item by item.
copy_image "$v5" "$SANDBOX/q.img"
fs.xfs "$SANDBOX/q.img" import --recursive "$tree" /t >"$SANDBOX/q.json" 2>/dev/null
check "a tree with links but no --preserve=links exits nonzero" test $? -ne 0
jq_check "the symlinks are refused, and nothing failed" '.failed == 0 and .refused == 2' "$SANDBOX/q.json"

finish
