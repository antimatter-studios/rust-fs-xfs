# fs.xfs export and import --preserve=xattrs, as installed. On Linux a
# host file's user. attributes import, read back with the xattr verb, and
# export back to the host with the same bytes; on macOS, whose attribute
# names have no namespace, an attribute XFS cannot hold refuses its item
# and is never dropped. What the kernel lists is
# tests/cli_xattr_transfer_kernel.rs. A transfer that cannot carry the
# attributes is a usage error, not a copy without them.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img

v5="$SHARE/xfscli-v5.img"
tree="$SANDBOX/tree"
mkdir -p "$tree"
echo data >"$tree/f"
copy_image "$v5" "$SANDBOX/x.img"
img="$SANDBOX/x.img"

fs.xfs "$img" import --preserve=xattrs "$tree/f" /one >/dev/null 2>"$SANDBOX/e"
check "a single-file import with --preserve=xattrs is a usage error" test $? -eq 2
check "the usage error names --recursive" grep -q -- --recursive "$SANDBOX/e"
fs.xfs "$img" export --recursive --preserve=xattrs,sparse / "$SANDBOX/no" >/dev/null 2>"$SANDBOX/e"
check "--preserve=xattrs,sparse is a usage error" test $? -eq 2
check "nothing was exported" test ! -e "$SANDBOX/no"

if [ "$(uname -s)" = Darwin ]; then
    xattr -w com.example.note hello "$tree/f"
    fs.xfs "$img" import --recursive --preserve=xattrs "$tree" /t >"$SANDBOX/r.json" 2>/dev/null
    check "an attribute XFS cannot hold fails the import" test $? -ne 0
    jq_check "the file is refused, not failed" \
        '[.items[] | select(.path | endswith("/f")) | .status] == ["refused"] and .failed == 0' "$SANDBOX/r.json"
    jq_check "the refusal names the namespaces XFS holds" \
        '[.items[] | select(.reason | test("namespaces are user, trusted and security"))] | length >= 1' "$SANDBOX/r.json"
else
    python3 -I -c 'import os, sys; os.setxattr(sys.argv[1], b"user.colour", b"blue"); os.setxattr(sys.argv[1], b"user.bin", b"\x00\xff")' "$tree/f"
    fs.xfs "$img" import --recursive --preserve=xattrs "$tree" /t >"$SANDBOX/r.json" 2>"$SANDBOX/e"
    rc=$?
    check "import --recursive --preserve=xattrs exits 0 ($(head -c 300 "$SANDBOX/e"))" test "$rc" -eq 0
    jq_check "every item completed" '.completed == 2 and .refused == 0 and .failed == 0' "$SANDBOX/r.json"
    check "user.colour reads back" test "$(fs.xfs "$img" xattr /t/f user.colour)" = blue
    fs.xfs "$img" export --recursive --preserve=xattrs /t "$SANDBOX/out" >"$SANDBOX/x.json" 2>/dev/null
    check "export --recursive --preserve=xattrs exits 0" test $? -eq 0
    check "the host reads the attributes back byte for byte" python3 -I -c \
        'import os, sys; assert os.getxattr(sys.argv[1], b"user.colour") == b"blue"; assert os.getxattr(sys.argv[1], b"user.bin") == b"\x00\xff"' \
        "$SANDBOX/out/f"
fi
fs.xfs "$img" get dirty --text >"$SANDBOX/dirty"
check "the volume is left clean" test "$(cat "$SANDBOX/dirty")" = false

finish
