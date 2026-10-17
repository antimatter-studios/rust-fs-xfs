# Read-only xattr inspection, as installed. Linux-created attributes and
# their exact values are checked in tests/cli_xattr_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img
img="$SANDBOX/xattr.img"
copy_image "$SHARE/xfscli-v5.img" "$img"
cp "$img" "$SANDBOX/before.img"

for volume in "$img" "$SHARE/xfscli-v4.img"; do
    fs.xfs "$volume" xattr /small.txt >"$SANDBOX/list" 2>"$SANDBOX/err"
    check "an unadorned file has no attributes" test $? -eq 0
    jq_check "no attributes is an empty JSON array" '. == []' "$SANDBOX/list"
    check "successful listing has no stderr" test ! -s "$SANDBOX/err"
    fs.xfs "$volume" xattr --text /small.txt >"$SANDBOX/list" 2>"$SANDBOX/err"
    check "text listing succeeds" test $? -eq 0
    check "no attributes has no text rows" test ! -s "$SANDBOX/list"
done

for args in '/small.txt user.absent' '/absent' '/absent user.absent'; do
    # Intentional splitting: these fixed arguments contain no spaces.
    # shellcheck disable=SC2086
    fs.xfs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "missing path or attribute exits 1: $args" test $? -eq 1
    check "missing path or attribute has no stdout: $args" test ! -s "$SANDBOX/out"
    jq_check "missing path or attribute has a structured error" \
        '.code == 1 and (.error | length > 0)' "$SANDBOX/err"
done
fs.xfs "$img" xattr /small.txt user.absent >"$SANDBOX/out" 2>"$SANDBOX/err"
check "missing attribute error names the attribute" grep -q 'user.absent' "$SANDBOX/err"
printf keep >"$SANDBOX/value"
fs.xfs "$img" xattr /small.txt user.absent -o "$SANDBOX/value" >"$SANDBOX/out" 2>"$SANDBOX/err"
check "missing attribute with -o exits 1" test $? -eq 1
check "missing attribute preserves an existing output" test "$(cat "$SANDBOX/value")" = keep
check "missing attribute creates no partial output" test ! -e "$SANDBOX/value.partial"
for args in '' '/small.txt -o nowhere' '/small.txt user.a extra'; do
    # shellcheck disable=SC2086
    fs.xfs "$img" xattr $args >"$SANDBOX/out" 2>"$SANDBOX/err"
    check "invalid xattr arguments exit 2: $args" test $? -eq 2
    check "invalid arguments have no stdout" test ! -s "$SANDBOX/out"
    jq_check "invalid arguments have a structured usage error" '.code == 2' "$SANDBOX/err"
done
check "inspection and refusals preserve the image" cmp -s "$img" "$SANDBOX/before.img"

finish
