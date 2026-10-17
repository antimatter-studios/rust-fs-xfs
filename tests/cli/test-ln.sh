# ln, as installed: the same inode under two names, literal symlink
# targets, clean release, and byte-identical refusals. Linux and xfs_repair
# judge the resulting metadata in cli_ln_kernel.rs.
source "$(dirname "$0")/lib.sh"
need_fixture xfscli-v5.img xfscli-v4.img xfscli-maxlinks.img xfsdirty.img
img="$SANDBOX/links.img"
copy_image "$SHARE/xfscli-v5.img" "$img"

fs.xfs "$img" ln /small.txt /sub/also >"$SANDBOX/link.json" 2>"$SANDBOX/e"
rc=$?
check "hard link exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
jq_check "hard link reports the shared keys" \
    '.path == "/sub/also" and .linked == true and (.inode | type) == "number" and (keys | length) == 3' "$SANDBOX/link.json"
for p in /small.txt /sub/also; do
    check "$p reads the original bytes" test "$(fs.xfs "$img" read "$p")" = 'hello world'
    fs.xfs "$img" stat "$p" >"$SANDBOX/stat.json" 2>/dev/null
    jq_check "$p has two links" '.nlink == 2' "$SANDBOX/stat.json"
    check "$p has the reported inode" test "$(jq .inode "$SANDBOX/stat.json")" = "$(jq .inode "$SANDBOX/link.json")"
done
check "hard link releases clean" test "$(fs.xfs "$img" get dirty --text)" = false

long="$(printf '%01023d' 0)"
for target in ../small.txt /small.txt 'missing/../literal text' "$long"; do
    dest="/sub/sym-$passed"
    fs.xfs "$img" ln -s "$target" "$dest" >"$SANDBOX/link.json" 2>"$SANDBOX/e"
    rc=$?
    check "symlink exits 0 ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
    jq_check "symlink reports path, linked and inode" \
        ".path == \"$dest\" and .linked == true and (.inode | type) == \"number\" and (keys | length) == 3" "$SANDBOX/link.json"
    fs.xfs "$img" ls "$dest" >"$SANDBOX/ls.json" 2>/dev/null
    check "symlink preserves its target text" test "$(jq -r '.[0].target' "$SANDBOX/ls.json")" = "$target"
    check "symlink releases clean" test "$(fs.xfs "$img" get dirty --text)" = false
done
fs.xfs "$img" ln --text /empty /empty-twin >"$SANDBOX/text" 2>/dev/null
check "ln --text describes the new name" test "$(cat "$SANDBOX/text")" = 'linked /empty-twin'
# A hard link to a symlink names that inode, without following it.
fs.xfs "$img" ln /link-short /sym-twin >"$SANDBOX/link.json" 2>/dev/null
check "hard link to symlink succeeds" test $? -eq 0
fs.xfs "$img" stat /sym-twin >"$SANDBOX/stat.json" 2>/dev/null
jq_check "hard link preserves the symlink inode" '.type == "symlink" and .nlink == 2' "$SANDBOX/stat.json"

refused() {
    local code="$1" pattern="$2" base="$3"
    shift 3
    copy_image "$base" "$SANDBOX/refused.img"
    fs.xfs "$SANDBOX/refused.img" ln "$@" >"$SANDBOX/out" 2>"$SANDBOX/e"
    rc=$?
    check "ln $* exits $code ($(cat "$SANDBOX/e"))" test "$rc" -eq "$code"
    check "ln $* prints nothing on stdout" test ! -s "$SANDBOX/out"
    jq_check "ln $* explains refusal" ".code == $code and (.error | test(\"$pattern\"))" "$SANDBOX/e"
    check "ln $* leaves image byte-identical" cmp -s "$base" "$SANDBOX/refused.img"
}
v5="$SHARE/xfscli-v5.img"
refused 1 'already exists' "$v5" /small.txt /empty
refused 1 'already exists' "$v5" -s anywhere /link-short
refused 1 'no such file' "$v5" /small.txt /missing/name
refused 1 'no such file' "$v5" -s anywhere /missing/name
refused 1 'no such file' "$v5" /missing /new
refused 1 'not a directory' "$v5" /small.txt /small.txt/name
refused 1 'not a directory' "$v5" /small.txt /link-short/name
refused 1 'not a directory' "$v5" /small.txt /new/
refused 3 'directory' "$v5" /sub /new
refused 3 'most.*links|links.*most' "$SHARE/xfscli-maxlinks.img" /small.txt /new
refused 3 'v4 filesystem is not supported' "$SHARE/xfscli-v4.img" /small.txt /new
refused 3 'v4 filesystem is not supported' "$SHARE/xfscli-v4.img" -s anywhere /new
refused 3 'target.*1 to 1023' "$v5" -s '' /new
refused 3 'target.*1 to 1023' "$v5" -s "${long}x" /new
refused 3 'name' "$v5" /small.txt "/$(printf '%0256d' 0)"
refused 1 'log holds records' "$SHARE/xfsdirty.img" /whatever /new
refused 1 'log holds records' "$SHARE/xfsdirty.img" -s anywhere /new
refused 2 'required' "$v5" /small.txt

# The offset is the start of the filesystem, never a position in a file.
disk="$SANDBOX/disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
fs.xfs --offset 1048576 "$disk" ln /small.txt /offset-link >"$SANDBOX/link.json" 2>"$SANDBOX/e"
rc=$?
check "ln through --offset succeeds ($(cat "$SANDBOX/e"))" test "$rc" -eq 0
check "offset hard link reads the bytes" test "$(fs.xfs --offset 1048576 "$disk" read /offset-link)" = 'hello world'
check "offset ln releases clean" test "$(fs.xfs --offset 1048576 "$disk" get dirty --text)" = false
check "ln leaves the prefix untouched" test "$(head -c 1048576 "$disk" | tr -d '\0' | wc -c | tr -d ' ')" -eq 0
finish
