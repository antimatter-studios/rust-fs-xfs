#!/usr/bin/env bash
# The release tarball is an install prefix -- bin/rust-fs-xfs, each dotted
# name a relative symlink to it, share/rust-fs-xfs/CAVEATS and LICENSE,
# nothing else -- and every name in it runs and identifies itself.
#
# This runs the real packaging script against stand-in binaries in a
# sandbox: one that behaves, and one for each way a build can be wrong
# (missing, --help failing, reporting a version other than the tag's,
# listing no names, a CAVEATS too long for an installer to print). The
# release workflow and the `cli` CI job run the same script against the
# real binary, so the checks here are the checks a release makes.
set -uo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PACKAGE="$ROOT/scripts/package-cli.sh"
pass=0
fail=0

ok()  { pass=$((pass + 1)); }
bad() { fail=$((fail + 1)); printf 'FAIL  %s\n' "$*" >&2; }

mkdir -p "$ROOT/tmp"
sandbox="$(mktemp -d "$ROOT/tmp/package-cli.XXXXXX")"
trap 'rm -rf "$sandbox"' EXIT HUP INT TERM

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$ROOT/Cargo.toml" | head -n 1)"
[ "$crate" = "am-fs-xfs" ] && ok || bad "crate name read from Cargo.toml: '$crate'"

# A stand-in for the built multi-call binary: it answers under whatever
# name it was started as, like the real one. $1 is the version it
# reports, $2 the exit status of --help, $3 the names it lists, $4 where
# to put it (a directory named for the case).
stub() {
    local dir="$sandbox/$4"
    mkdir -p "$dir"
    cat > "$dir/rust-fs-xfs" <<STUB
#!/usr/bin/env bash
me="\$(basename "\$0")"
case "\$1" in
    --help)    echo "Usage: \$me [options]"; exit $2 ;;
    --version) echo "\$me ($crate) $1" ;;
    generate)  [ -z "$3" ] || printf '%s\n' $3 ;;
    *)         exit 2 ;;
esac
STUB
    chmod +x "$dir/rust-fs-xfs"
    printf '%s\n' "$dir"
}

# Runs the packaging script in a fresh output directory and prints the
# tarball's absolute path. CAVEATS_FROM=<file> packages with that CAVEATS
# instead (the script reads it from the repository, so the run is given
# a copy of the repository's files with that one swapped).
package() {
    local out="$sandbox/out-$RANDOM$RANDOM" name status script="$PACKAGE"
    mkdir -p "$out"
    printf '%s\n' "$out" > "$sandbox/package-out"
    if [ -n "${CAVEATS_FROM:-}" ]; then
        local fake="$sandbox/repo-$RANDOM"
        mkdir -p "$fake/scripts" "$fake/packaging"
        cp "$ROOT/Cargo.toml" "$ROOT/LICENSE" "$fake/"
        cp "$PACKAGE" "$fake/scripts/"
        cp "$CAVEATS_FROM" "$fake/packaging/CAVEATS"
        script="$fake/scripts/package-cli.sh"
    fi
    name="$(cd "$out" && bash "$script" "$@" 2>"$sandbox/stderr")"
    status=$?
    if [ "$status" -ne 0 ]; then
        printf '%s' "$name"
        return "$status"
    fi
    printf '%s\n' "$out/$name"
}

[ -f "$PACKAGE" ] && ok || bad "scripts/package-cli.sh exists"
lines="$(wc -l < "$ROOT/packaging/CAVEATS")"
[ "$lines" -ge 1 ] && [ "$lines" -le 4 ] && ok || bad "packaging/CAVEATS is 1 to 4 lines, is $lines"

# --- A good build: the tarball, its name, and exactly its contents. -------
good="$(stub 9.9.9 0 fs.xfs good)"
if tarball="$(package 9.9.9 darwin-arm64 "$good")"; then
    ok
else
    bad "a good build packages: $(cat "$sandbox/stderr")"
    tarball=""
fi

case "$(basename "$tarball")" in
    "$crate-9.9.9-darwin-arm64.tar.gz") ok ;;
    *) bad "tarball is named <crate>-<version>-<label>.tar.gz, got '$tarball'" ;;
esac

# The content checks need the tarball. Without it they fail rather than
# fall silent, since a check that does not run reads like one that passed.
[ -f "$tarball" ] && ok || bad "the packaged tarball exists at '$tarball'"
if [ -f "$tarball" ]; then
    files="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | sort | tr '\n' ' ')"
    [ "$files" = "LICENSE bin/fs.xfs bin/rust-fs-xfs share/rust-fs-xfs/CAVEATS " ] && ok \
        || bad "tarball holds exactly the binary, its link, the CAVEATS and the licence, got: $files"
    unpacked="$sandbox/unpacked"
    mkdir -p "$unpacked"
    tar -xzf "$tarball" -C "$unpacked"
    [ -f "$unpacked/bin/rust-fs-xfs" ] && [ ! -L "$unpacked/bin/rust-fs-xfs" ] && ok \
        || bad "bin/rust-fs-xfs is the real file"
    [ -L "$unpacked/bin/fs.xfs" ] && [ "$(readlink "$unpacked/bin/fs.xfs")" = rust-fs-xfs ] && ok \
        || bad "bin/fs.xfs is a relative symlink to rust-fs-xfs"
    [ "$("$unpacked/bin/fs.xfs" --version)" = "fs.xfs ($crate) 9.9.9" ] && ok \
        || bad "bin/fs.xfs answers as fs.xfs"
    cmp -s "$unpacked/share/rust-fs-xfs/CAVEATS" "$ROOT/packaging/CAVEATS" && ok \
        || bad "share/rust-fs-xfs/CAVEATS is packaging/CAVEATS"
    cmp -s "$unpacked/LICENSE" "$ROOT/LICENSE" && ok || bad "LICENSE is the repository's"
    cmp -s "$unpacked/bin/rust-fs-xfs" "$good/rust-fs-xfs" && ok || bad "bin/rust-fs-xfs is the built binary"
fi

# --- Each way a build can be wrong is refused, with no tarball left. ------
refused() {
    local why="$1"; shift
    local stdout out_dir left
    if stdout="$(package "$@")"; then
        bad "$why is refused, but packaging succeeded: $stdout"
    else
        ok
        [ -z "$stdout" ] && ok || bad "$why leaves no tarball named on stdout: $stdout"
        out_dir="$(cat "$sandbox/package-out")"
        left="$(find "$out_dir" -maxdepth 1 -name '*.tar.gz')"
        [ -z "$left" ] && ok || bad "$why leaves no tarball behind, found: $left"
    fi
}

refused "a missing binary" 9.9.9 darwin-arm64 "$sandbox/nowhere"
refused "a binary whose --help fails" 9.9.9 darwin-arm64 "$(stub 9.9.9 1 fs.xfs helpfails)"
refused "a binary reporting a version other than the tag's" 9.9.9 darwin-arm64 "$(stub 1.0.0 0 fs.xfs wrongver)"
refused "a binary that lists no dotted names" 9.9.9 darwin-arm64 "$(stub 9.9.9 0 "" nonames)"
refused "a missing label" 9.9.9 "" "$good"
refused "a missing version" "" darwin-arm64 "$good"
printf 'one\ntwo\nthree\nfour\nfive\n' > "$sandbox/long-caveats"
CAVEATS_FROM="$sandbox/long-caveats" refused "a CAVEATS longer than four lines" 9.9.9 darwin-arm64 "$good"

# --- The workflows package through this script, and the release attests. --
release="$ROOT/.github/workflows/release.yml"
ci="$ROOT/.github/workflows/ci.yml"
grep -q 'scripts/package-cli.sh' "$release" && ok \
    || bad "release.yml packages through scripts/package-cli.sh"
grep -q 'cargo build --release --locked --features cli --bin rust-fs-xfs' "$release" && ok \
    || bad "release.yml builds the rust-fs-xfs target with the cli feature"
grep -q 'scripts/package-cli.sh' "$ci" && ok \
    || bad "ci.yml builds the tarball on every pull request, through scripts/package-cli.sh"
grep -qE 'subject-path: dist/\*\.tar\.gz' "$release" && ok \
    || bad "release.yml attests the tarballs' build provenance"

if [ "$fail" -gt 0 ]; then
    echo "FAIL  package-cli: $pass passed, $fail failed" >&2
    exit 1
fi
echo "PASS  the release tarball is an install prefix and every name in it answers ($pass checks)"
