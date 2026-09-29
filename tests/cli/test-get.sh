# fs.xfs get and info on the kernel-made images: every canonical key with
# its JSON type, the label mkfs.xfs was given, get and info identical, one
# key by name, --offset into a larger file, the verbs that answer `not
# implemented`, and structured errors with nothing on stdout.
#
# What xfs_db and xfs_info say about the same superblock is compared in
# tests/cli_get_oracle.rs: this file checks the tool's contract, that one
# checks its answers.
source "$(dirname "$0")/lib.sh"

need_fixture xfscli-v5.img xfscli-v4.img xfscli-v5.corrupt

for spec in v5:CLIV5:5 v4:CLIV4:4; do
    name="${spec%%:*}"
    rest="${spec#*:}"
    label="${rest%%:*}"
    version="${rest#*:}"
    img="$SHARE/xfscli-$name.img"

    fs.xfs "$img" get >"$SANDBOX/get.json" 2>"$SANDBOX/get.err"
    rc=$?
    check "$name: get exits 0 ($(cat "$SANDBOX/get.err"))" test "$rc" -eq 0
    jq_check "$name: get carries every canonical key with its type" \
        '(.fs=="xfs") and (.label|type)=="string" and (.total_bytes|type)=="number" and (.free_bytes|type)=="number" and (.block_size|type)=="number" and (.dirty|type)=="boolean" and (.xfs|type)=="object"' \
        "$SANDBOX/get.json"
    jq_check "$name: the label is the one mkfs.xfs was given" ".label == \"$label\"" "$SANDBOX/get.json"
    jq_check "$name: the format version is $version" ".xfs.version == $version" "$SANDBOX/get.json"
    jq_check "$name: a cleanly unmounted image is not dirty" '.dirty == false' "$SANDBOX/get.json"
    jq_check "$name: the sizes are whole blocks, free within total" \
        '(.total_bytes % .block_size == 0) and (.free_bytes <= .total_bytes) and (.total_bytes == .xfs.total_blocks * .block_size)' \
        "$SANDBOX/get.json"
    jq_check "$name: the nested numbers are numbers" \
        '[.xfs | .ag_count, .ag_blocks, .total_blocks, .free_blocks, .inode_count, .free_inodes, .sector_size, .inode_size, .root_inode] | all(type == "number")' \
        "$SANDBOX/get.json"
    fs.xfs "$img" info >"$SANDBOX/info.json" 2>/dev/null
    check "$name: info and get print the same" cmp -s "$SANDBOX/get.json" "$SANDBOX/info.json"
    fs.xfs "$img" get label >"$SANDBOX/label.json" 2>/dev/null
    jq_check "$name: get label is {\"label\": \"$label\"}" ". == {\"label\": \"$label\"}" "$SANDBOX/label.json"
    check "$name: get label --text is $label" test "$(fs.xfs "$img" get label --text)" = "$label"
    check "$name: get xfs.uuid --text is a UUID" \
        grep -qE '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$' <<<"$(fs.xfs "$img" get xfs.uuid --text)"
    check "$name: rust-fs-xfs fs is the same program" \
        test "$(rust-fs-xfs fs "$img" get label --text)" = "$label"

    # A key that does not exist is a wrong command line.
    fs.xfs "$img" get nonsense >"$SANDBOX/nk.out" 2>"$SANDBOX/nk.err"
    check "$name: get of an unknown key exits 2" test $? -eq 2
    check "$name: get of an unknown key prints nothing on stdout" test ! -s "$SANDBOX/nk.out"
    jq_check "$name: get of an unknown key names the keys" '.code == 2 and (.error | test("label"))' "$SANDBOX/nk.err"

    # Verbs the library cannot do: status 3, `not implemented`, nothing on stdout.
    for verb in "set label X" "resize 1G"; do
        # shellcheck disable=SC2086  # the words are the point
        fs.xfs "$img" $verb >"$SANDBOX/ni.out" 2>"$SANDBOX/ni.err"
        check "$name: $verb exits 3" test $? -eq 3
        check "$name: $verb prints nothing on stdout" test ! -s "$SANDBOX/ni.out"
        jq_check "$name: $verb says not implemented" '.code == 3 and (.error | startswith("not implemented"))' "$SANDBOX/ni.err"
    done
    fs.xfs "$img" set total_bytes 1 >"$SANDBOX/ro.out" 2>"$SANDBOX/ro.err"
    check "$name: set of a derived key exits 3" test $? -eq 3
    jq_check "$name: set of a derived key says it is read-only" '.code == 3 and (.error | test("read-only"))' "$SANDBOX/ro.err"
done

# --offset: the v5 image one MiB into a larger file, as a partition sits
# in a whole-disk image. Written sparse, so the copy costs no more than
# the image's own blocks.
v5="$SHARE/xfscli-v5.img"
disk="$SANDBOX/disk.img"
dd if=/dev/zero of="$disk" bs=1048576 count=1 status=none
dd if="$v5" of="$disk" bs=1048576 seek=1 conv=sparse,notrunc status=none
check "--offset 1048576 finds the label" \
    test "$(fs.xfs --offset 1048576 "$disk" get label --text 2>&1)" = CLIV5
check "--offset may follow the verb" \
    test "$(fs.xfs "$disk" get label --text --offset 1048576 2>&1)" = CLIV5
fs.xfs "$disk" get >"$SANDBOX/nooff.out" 2>"$SANDBOX/nooff.err"
check "without --offset the same file is not XFS (exit 1)" test $? -eq 1
check "without --offset nothing is printed on stdout" test ! -s "$SANDBOX/nooff.out"
jq_check "without --offset the error says it is not XFS" '.code == 1 and (.error | test("not an XFS volume"))' "$SANDBOX/nooff.err"
fs.xfs --offset 999999999999 "$disk" get >"$SANDBOX/past.out" 2>"$SANDBOX/past.err"
check "--offset past the end exits 1" test $? -eq 1
jq_check "--offset past the end says so" '.code == 1 and (.error | test("past the end"))' "$SANDBOX/past.err"

# Failures: status 1, a structured error, nothing on stdout, no panic.
fs.xfs "$SANDBOX/absent.img" get >"$SANDBOX/absent.out" 2>"$SANDBOX/absent.err"
check "a missing image exits 1" test $? -eq 1
check "a missing image prints nothing on stdout" test ! -s "$SANDBOX/absent.out"
jq_check "a missing image is a structured error" '.code == 1' "$SANDBOX/absent.err"
head -c 4096 "$v5" >"$SANDBOX/cut.img"
fs.xfs "$SANDBOX/cut.img" info >"$SANDBOX/cut.out" 2>"$SANDBOX/cut.err"
check "a truncated image exits 1" test $? -eq 1
check "a truncated image prints nothing on stdout" test ! -s "$SANDBOX/cut.out"
jq_check "a truncated image is a structured error" '.code == 1' "$SANDBOX/cut.err"

# An AG header with a bad magic: the mount reads every AGI, so get fails.
at="$(awk -F'\t' '$1 == "agi-magic" { print $2 }' "$SHARE/xfscli-v5.corrupt")"
check "the .corrupt file locates the AGI" test -n "$at"
copy_image "$v5" "$SANDBOX/badagi.img"
poke "$SANDBOX/badagi.img" "$at"
fs.xfs "$SANDBOX/badagi.img" get >"$SANDBOX/agi.out" 2>"$SANDBOX/agi.err"
check "a bad AGI magic exits 1" test $? -eq 1
check "a bad AGI magic prints nothing on stdout" test ! -s "$SANDBOX/agi.out"
jq_check "a bad AGI magic is a structured error, not a panic" '.code == 1 and (.error | type == "string")' "$SANDBOX/agi.err"
check "a bad AGI magic does not panic" test "$(grep -c panicked "$SANDBOX/agi.err")" -eq 0

finish
