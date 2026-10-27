# fs.xfs import, as installed, on copies of the kernel-made images: a local
# file, stdin and a local tree are streamed into the image and read back by
# `read` (a file the kernel made, digested in its manifest, proves an import
# did not disturb its neighbours). A file is published only when complete: every refusal and
# every failure leaves the destination as it was and no staged name behind.
#
# What the kernel and xfs_repair make of the imported volume, a 64 MiB file,
# a nearly full image and a killed import is tests/cli_import_kernel.rs.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img xfscli-v5.manifest

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum | cut -d' ' -f1
    else
        shasum -a 256 | cut -d' ' -f1
    fi
}

v5="$SHARE/xfscli-v5.img"
v4="$SHARE/xfscli-v4.img"
manifest="$SHARE/xfscli-v5.manifest"
n=0
fresh() {
    n=$((n + 1))
    copy_image "$1" "$SANDBOX/i$n.img"
    printf '%s\n' "$SANDBOX/i$n.img"
}
staged() { fs.xfs "$1" ls / | jq -r '.[].name' | grep -c '^\.fs-xfs-import-'; }

head -c 3145729 /dev/urandom >"$SANDBOX/b3m1.bin"
printf 'x' >"$SANDBOX/one.bin"
: >"$SANDBOX/empty.bin"

# A file, a byte, nothing, and stdin: each reads back as it was sent, and
# the report says so.
img="$(fresh "$v5")"
for src in b3m1.bin one.bin empty.bin; do
    want="$(sha256 <"$SANDBOX/$src")"
    size="$(wc -c <"$SANDBOX/$src" | tr -d ' ')"
    fs.xfs "$img" import --verify "$SANDBOX/$src" "/in-$src" >"$SANDBOX/r.json" 2>"$SANDBOX/r.err"
    rc=$?
    check "import $src exits 0 ($(cat "$SANDBOX/r.err"))" test "$rc" -eq 0
    check "import $src reads back as the file sent" \
        test "$(fs.xfs "$img" read "/in-$src" | sha256)" = "$want"
    jq_check "import $src reports $size bytes, the digest, created and verified" \
        ".bytes == $size and .sha256 == \"$want\" and .created == true and .replaced == false and .verified == true" \
        "$SANDBOX/r.json"
done
fs.xfs "$img" import - /from-stdin <"$SANDBOX/b3m1.bin" >/dev/null 2>&1
check "import from stdin reads back as the bytes sent" \
    test "$(fs.xfs "$img" read /from-stdin | sha256)" = "$(sha256 <"$SANDBOX/b3m1.bin")"
check "no staged name is left after the imports" test "$(staged "$img")" -eq 0
check "a file the kernel made is untouched" \
    test "$(fs.xfs "$img" read /small.txt | sha256)" = "$(awk -F'\t' '$1 == "/small.txt" { print $6 }' "$manifest")"
fs.xfs "$img" get dirty --text >"$SANDBOX/dirty"
check "the volume is left clean" test "$(cat "$SANDBOX/dirty")" = false

# An existing name is refused without --replace and replaced with it, in
# one rename: the old bytes are what is there until it happens.
img="$(fresh "$v5")"
old="$(fs.xfs "$img" read /small.txt | sha256)"
fs.xfs "$img" import "$SANDBOX/one.bin" /small.txt >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
check "import onto an existing file exits 1" test $? -eq 1
check "import onto an existing file prints nothing on stdout" test ! -s "$SANDBOX/r.out"
jq_check "import onto an existing file says to give --replace" '.code == 1 and (.error | test("--replace"))' "$SANDBOX/r.err"
check "a refused import leaves the file as it was" test "$(fs.xfs "$img" read /small.txt | sha256)" = "$old"
check "a refused import leaves no staged name" test "$(staged "$img")" -eq 0
fs.xfs "$img" import --replace "$SANDBOX/b3m1.bin" /small.txt >"$SANDBOX/r.json" 2>/dev/null
check "import --replace exits 0" test $? -eq 0
jq_check "import --replace says it replaced" '.replaced == true and .created == false' "$SANDBOX/r.json"
check "import --replace puts the new bytes there" \
    test "$(fs.xfs "$img" read /small.txt | sha256)" = "$(sha256 <"$SANDBOX/b3m1.bin")"
check "import --replace leaves no staged name" test "$(staged "$img")" -eq 0

# Refusals, each leaving the image byte-identical.
img="$(fresh "$v5")"
before="$(sha256 <"$img")"
while IFS='|' read -r what code args; do
    # shellcheck disable=SC2086  # $args is a deliberate word split
    fs.xfs "$img" import $args >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
    check "import $what exits $code" test $? -eq "$code"
    check "import $what prints nothing on stdout" test ! -s "$SANDBOX/r.out"
    check "import $what leaves the image byte-identical" test "$(sha256 <"$img")" = "$before"
done <<EOF
a missing source|1|$SANDBOX/none /x
a missing directory for the destination|4|$SANDBOX/one.bin /nodir/x
a destination with no name|2|$SANDBOX/one.bin /
a directory source without --recursive|1|$SANDBOX /x
--verify with --recursive|2|--verify --recursive $SANDBOX /x
--preserve=sparse, which this sink cannot hold|3|--preserve=sparse $SANDBOX/one.bin /x
the image as its own source|1|$img /x
EOF
fs.xfs "$img" import "$SANDBOX/one.bin" /small.txt/x >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
jq_check "import below a file says it is not a directory" '.code == 4 and (.error | test("not a directory"))' "$SANDBOX/r.err"

# v4 is refused as a shape (exit 3), the image untouched.
img="$(fresh "$v4")"
before="$(sha256 <"$img")"
fs.xfs "$img" import "$SANDBOX/one.bin" /x >"$SANDBOX/r.out" 2>"$SANDBOX/r.err"
check "import into v4 exits 3" test $? -eq 3
check "import into v4 leaves the image byte-identical" test "$(sha256 <"$img")" = "$before"

# A tree: directories made, files read back, a symlink refused without
# --preserve=links, every item reported.
mkdir -p "$SANDBOX/tree/a/b" "$SANDBOX/tree/empty-dir"
head -c 300000 /dev/urandom >"$SANDBOX/tree/a/b/f.bin"
echo top >"$SANDBOX/tree/top.txt"
ln -s top.txt "$SANDBOX/tree/link"
img="$(fresh "$v5")"
fs.xfs "$img" import --recursive "$SANDBOX/tree" /tree >"$SANDBOX/t.json" 2>/dev/null
check "a tree with a symlink exits nonzero" test $? -ne 0
jq_check "the tree report has one refused item, the symlink, and no failure" \
    '.failed == 0 and .refused == 1 and ([.items[] | select(.status == "refused") | .path] | length == 1)' "$SANDBOX/t.json"
check "tree: a nested file reads back" \
    test "$(fs.xfs "$img" read /tree/a/b/f.bin | sha256)" = "$(sha256 <"$SANDBOX/tree/a/b/f.bin")"
check "tree: a top file reads back" test "$(fs.xfs "$img" read /tree/top.txt)" = top
check "tree: an empty directory is made" \
    test "$(fs.xfs "$img" ls /tree/empty-dir | jq length)" -eq 0
check "tree: the symlink was not imported" test "$(fs.xfs "$img" ls /tree | jq '[.[] | select(.name == "link")] | length')" -eq 0
check "tree: no staged name is left" test "$(staged "$img")" -eq 0
# Importing it again merges the directories and fails each file that is there, asking for --replace.
fs.xfs "$img" import --recursive "$SANDBOX/tree" /tree >"$SANDBOX/t2.json" 2>/dev/null
check "importing the tree again exits nonzero" test $? -ne 0
jq_check "the second import fails the two files that exist, asking for --replace" \
    '.failed == 2 and ([.items[] | select(.status == "failed") | .reason | select(test("--replace"))] | length == 2)' "$SANDBOX/t2.json"

fs.xfs "$v5" import --help >"$SANDBOX/help" 2>&1
check "import --help carries an example" grep -q 'Examples:' "$SANDBOX/help"

finish
