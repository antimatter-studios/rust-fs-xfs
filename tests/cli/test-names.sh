# Every name this repository installs resolves on PATH, answers --version
# as itself and this crate, at the one version the entry point reports,
# and carries an example in its --help. And the names it does NOT install
# stay uninstalled: no growfs.xfs, because this crate cannot grow a volume.
source "$(dirname "$0")/lib.sh"

# The names are written here, not read from the binary: a binary that
# forgot one would otherwise agree with itself.
EXPECTED="fs.xfs mkfs.xfs fsck.xfs"

version="$(rust-fs-xfs --version | sed -n "s/^rust-fs-xfs ($CRATE) //p")"
check "rust-fs-xfs --version names a version" test -n "$version"

listed="$(rust-fs-xfs generate names | tr '\n' ' ' | sed 's/ $//')"
check "rust-fs-xfs generate names lists exactly '$EXPECTED' (got '$listed')" \
    test "$listed" = "$EXPECTED"

for name in $EXPECTED rust-fs-xfs; do
    path="$(command -v "$name" 2>/dev/null || true)"
    if [ -z "$path" ]; then
        fail "$name is not on PATH"
        continue
    fi
    ok
    for flag in --version -V; do
        got="$("$name" "$flag" 2>&1)"
        check "$path $flag answered '$got', not '$name ($CRATE) $version'" \
            test "$got" = "$name ($CRATE) $version"
    done
    help="$("$name" --help 2>&1)"
    check "$name --help carries no example" grep -q '^Examples:' <<<"$help"
done

# The repository-named form reaches every tool, and nothing can shadow it.
for name in $EXPECTED; do
    verb="${name%%.*}"
    got="$(rust-fs-xfs "$verb" --version 2>&1)"
    check "rust-fs-xfs $verb --version answered '$got'" test "$got" = "$name ($CRATE) $version"
done

# No growfs: the verb does not exist on the entry point, and the staged
# prefix does not link the name.
bin="$(dirname "$(command -v rust-fs-xfs)")"
for missing in growfs.xfs; do
    check "$bin holds no $missing" test ! -e "$bin/$missing"
done
for verb in growfs; do
    rust-fs-xfs "$verb" >"$SANDBOX/$verb.out" 2>"$SANDBOX/$verb.err"
    check "rust-fs-xfs $verb is refused as a wrong command line (exit 2)" test $? -eq 2
    jq_check "rust-fs-xfs $verb is a structured error" '.code == 2' "$SANDBOX/$verb.err"
done

# A bare entry point shows its help and says nothing was done.
rust-fs-xfs >"$SANDBOX/bare.out" 2>&1
check "a bare rust-fs-xfs exits 2" test $? -eq 2
check "a bare rust-fs-xfs lists the tools" grep -q 'fs.xfs' "$SANDBOX/bare.out"

finish
