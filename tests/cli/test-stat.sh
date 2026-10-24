# fs.xfs stat, as installed: the shared keys in the shared order and
# typed, xfs's own under `xfs`, the values `write` and `mkdir` produced,
# --text, the refusals, and an image left exactly as it was. What the
# values must be on a volume Linux wrote is tests/cli_stat_kernel.rs.
source "$(dirname "$0")/lib.sh"

img="$SANDBOX/stat.img"
need_fixture xfscli-v5.img xfscli-v4.img
copy_image "$SHARE/xfscli-v5.img" "$img"
# Each journalled write needs a clean log: give mkdir and write their own copy.
file_img="$SANDBOX/file.img"
copy_image "$img" "$file_img"
fs.xfs "$img" mkdir /d >/dev/null 2>&1
check "mkdir /d exits 0" test $? -eq 0
head -c 5000 /dev/urandom >"$SANDBOX/src"
fs.xfs "$file_img" write /f <"$SANDBOX/src" >/dev/null 2>&1
check "write /f exits 0" test $? -eq 0
cp "$img" "$SANDBOX/before.img"

shared='["path","inode","type","mode","uid","gid","nlink","size","allocated_bytes","atime","atime_nsec","mtime","mtime_nsec","ctime","ctime_nsec","btime","btime_nsec"]'
typed='(.path|type)=="string" and (.inode|type)=="number" and (.type|type)=="string"
    and (.mode|test("^[0-7]{4}$")) and (.uid|type)=="number" and (.gid|type)=="number"
    and (.nlink|type)=="number" and (.size|type)=="number" and (.allocated_bytes|type)=="number"
    and (.atime|type)=="number" and (.mtime|type)=="number" and (.ctime|type)=="number"
    and all(.atime_nsec, .mtime_nsec, .ctime_nsec, .btime, .btime_nsec; type=="number" or type=="null")
    and (.xfs|type)=="object" and (.xfs.flags|type)=="array" and (.xfs.generation|type)=="number"'

for path in / /d /f; do
    at="$img"
    [ "$path" != /f ] || at="$file_img"
    fs.xfs "$at" stat "$path" >"$SANDBOX/stat.json" 2>"$SANDBOX/stat.err"
    rc=$?
    check "stat $path exits 0 ($(cat "$SANDBOX/stat.err"))" test "$rc" -eq 0
    jq_check "stat $path starts with the shared keys, in order" \
        "[keys_unsorted[]] | .[0:17] == $shared" "$SANDBOX/stat.json"
    jq_check "stat $path has every shared key typed" "$typed" "$SANDBOX/stat.json"
    jq_check "stat $path reports the path it was asked" ".path == \"$path\"" "$SANDBOX/stat.json"
done

fs.xfs "$img" stat / >"$SANDBOX/root.json" 2>/dev/null
jq_check "stat / is the root directory" '.type == "dir" and .inode > 0' "$SANDBOX/root.json"
fs.xfs "$img" stat /d >"$SANDBOX/d.json" 2>/dev/null
jq_check "stat /d is an empty directory: two links" '.type == "dir" and .nlink == 2' "$SANDBOX/d.json"
fs.xfs "$file_img" stat /f >"$SANDBOX/f.json" 2>/dev/null
jq_check "stat /f is a 5000-byte file with one link" \
    '.type == "file" and .size == 5000 and .nlink == 1 and .allocated_bytes >= 5000' "$SANDBOX/f.json"
fs.xfs "$file_img" ls / >"$SANDBOX/ls.json" 2>/dev/null
check "stat /f and ls / agree on the inode and the mode" \
    test "$(jq -c '[.inode, .mode]' "$SANDBOX/f.json")" = "$(jq -c '.[] | select(.name == "f") | [.inode, .mode]' "$SANDBOX/ls.json")"

fs.xfs "$file_img" stat --text /f >"$SANDBOX/f.txt" 2>/dev/null
check "stat --text /f exits 0" test $? -eq 0
check "stat --text /f prints key: value lines" grep -qx 'size: 5000' "$SANDBOX/f.txt"
check "stat --text /f names the type" grep -qx 'type: file' "$SANDBOX/f.txt"

# Refusals: a structured error, nothing on stdout.
fs.xfs "$img" stat /missing >"$SANDBOX/m.out" 2>"$SANDBOX/m.err"
check "stat of a missing path exits 1" test $? -eq 1
check "stat of a missing path prints nothing on stdout" test ! -s "$SANDBOX/m.out"
jq_check "stat of a missing path is the not_found kind" '.code == 1 and .kind == "not_found"' "$SANDBOX/m.err"
fs.xfs "$img" stat >"$SANDBOX/u.out" 2>"$SANDBOX/u.err"
check "stat with no path is a usage error" test $? -eq 2
check "stat with no path prints nothing on stdout" test ! -s "$SANDBOX/u.out"

check "stat left the image as it was" cmp -s "$img" "$SANDBOX/before.img"

fs.xfs "$SHARE/xfscli-v4.img" stat /small.txt >"$SANDBOX/v4.json" 2>/dev/null
jq_check "v4 has no creation time or flags2" '.btime == null and .btime_nsec == null and .xfs.flags2 == null' "$SANDBOX/v4.json"

finish
