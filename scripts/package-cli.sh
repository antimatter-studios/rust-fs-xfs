#!/usr/bin/env bash
# package-cli.sh <version> <label> [target-dir]
#
# Package the built command-line tools as a release tarball in the current
# directory, check it, and print its file name on stdout.
#
#   <version>     the release version, without the leading `v`
#   <label>       the platform, e.g. darwin-arm64 or linux-x86_64
#   [target-dir]  where cargo put the release build (default: target/release)
#
# THE TARBALL IS THE CONTRACT with whatever installs it, and every
# repository in the family lays it out the same way: a prefix, so an
# installer copies it as-is and needs to know nothing about which tools
# are in it.
#
#   bin/rust-fs-xfs             the multi-call binary (the real file)
#   bin/<dotted name>           -> rust-fs-xfs, a relative symlink, per tool
#   share/man/man1/<name>.1     a page per name, and per subcommand
#                               (section 8 is for mkfs.* and fsck.*, and
#                               this crate ships neither)
#   share/zsh/site-functions/_<name>
#   share/bash-completion/completions/<name>
#   share/fish/vendor_completions.d/<name>.fish
#   share/rust-fs-xfs/CAVEATS   at most four lines an installer shows
#   LICENSE
#
# THE PAGES AND COMPLETIONS COME FROM THE BINARY (`rust-fs-xfs generate man
# SHARE`, `generate completions SHARE`), from the same clap commands it
# parses with, so they cannot describe a flag it does not take -- and the
# release needs no second build to make them.
#
# <repo> is the repository's name, from Cargo.toml's `repository`, and the
# tarball is named for the crate: am-fs-xfs-<version>-<label>.tar.gz.
#
# THE DOTTED NAMES ARE MADE HERE. Cargo refuses a dot in a target name, so
# the one target is the repository-named binary and each `fs.xfs` is a
# symlink to it; the binary dispatches on the name it was started under.
# The names come from the binary itself (`rust-fs-xfs generate names`), so
# nothing here has to be kept in step with the code.
#
# THEN IT CHECKS WHAT IT BUILT, because a tarball whose tools do not run is
# worse than no tarball: the failure would surface as a user's bug report
# rather than a red build. The member list must be exactly the intended
# one, every dotted name must be a relative symlink to the binary, CAVEATS
# must be at most four lines, and every name must answer --help and report
# `<name> (<crate>) <version>` from --version, which identifies it among
# same-named tools from other packages and catches a tag that disagrees
# with Cargo.toml. On any failure nothing is printed on stdout and no
# tarball is left behind. tests/scripts/test-package-cli.sh holds this
# script to all of that.
set -euo pipefail

version="${1:-}"
label="${2:-}"
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
target_dir="${3:-$root/target/release}"

licences=(LICENSE)

die() { echo "package-cli: $*" >&2; exit 1; }

[ -n "$version" ] || die "usage: package-cli.sh <version> <label> [target-dir]"
[ -n "$label" ] || die "usage: package-cli.sh <version> <label> [target-dir]"

crate="$(sed -n 's/^name = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
[ -n "$crate" ] || die "no package name in $root/Cargo.toml"
repo="$(sed -n 's/^repository = "\(.*\)"$/\1/p' "$root/Cargo.toml" | head -n 1)"
repo="${repo%/}"
repo="${repo##*/}"
[ -n "$repo" ] || die "no repository in $root/Cargo.toml"

tarball="$crate-$version-$label.tar.gz"
work="$(mktemp -d)"

# ON ANY FAILURE, NO TARBALL: not a partial one, and not one a previous run
# left under the same name, which a caller could otherwise take for this
# run's output.
cleanup() {
    local status=$?
    rm -rf "$work"
    [ "$status" -eq 0 ] || rm -f "$tarball"
    return "$status"
}
trap cleanup EXIT

built="$target_dir/$repo"
[ -x "$built" ] || die "no built $repo at $built (cargo build --release --locked --features cli --bin $repo)"

stage="$work/stage"
mkdir -p "$stage/bin" "$stage/share/$repo" "$work/unpacked"
cp "$built" "$stage/bin/$repo"
chmod 755 "$stage/bin/$repo"
want=("bin/$repo")

names="$("$stage/bin/$repo" generate names)" || die "$repo generate names failed"
[ -n "$names" ] || die "$repo lists no dotted names"
for name in $names; do
    ln -s "$repo" "$stage/bin/$name"
    want+=("bin/$name")
done

"$stage/bin/$repo" generate man "$stage/share" > /dev/null || die "$repo generate man failed"
"$stage/bin/$repo" generate completions "$stage/share" > /dev/null || die "$repo generate completions failed"
# Every name has its page and its three completions, where Homebrew links
# them from. Named here rather than read from what was generated, which
# would agree with itself whatever it wrote.
for name in $repo $names; do
    for doc in "man/man1/$name.1" "zsh/site-functions/_$name" \
        "bash-completion/completions/$name" "fish/vendor_completions.d/$name.fish"; do
        [ -s "$stage/share/$doc" ] || die "$repo generated no share/$doc"
    done
done
[ ! -e "$stage/share/man/man8" ] || die "$repo wrote section-8 pages, and this crate ships no mkfs.* or fsck.*"
while IFS= read -r doc; do
    want+=("${doc#"$stage/"}")
done < <(find "$stage/share" -type f ! -path "$stage/share/$repo/*" | sort)

caveats="$root/packaging/CAVEATS"
[ -s "$caveats" ] || die "packaging/CAVEATS is missing or empty"
[ "$(wc -l < "$caveats")" -le 4 ] || die "packaging/CAVEATS is $(wc -l < "$caveats") lines; an installer prints it, so at most four"
cp "$caveats" "$stage/share/$repo/CAVEATS"
want+=("share/$repo/CAVEATS")
for f in "${licences[@]}"; do
    cp "$root/$f" "$stage/$f"
    want+=("$f")
done

# COPYFILE_DISABLE keeps macOS tar from adding ._ AppleDouble members.
COPYFILE_DISABLE=1 tar -czf "$tarball" -C "$stage" bin share "${licences[@]}"

# Files and links only: whether a tar lists the directories themselves
# varies by tar.
want_list="$(printf '%s\n' "${want[@]}" | sort)"
got_list="$(tar -tzf "$tarball" | sed 's|^\./||' | grep -v '/$' | sort)"
[ "$got_list" = "$want_list" ] \
    || die "$tarball holds [$(echo $got_list)], expected [$(echo $want_list)]"

tar -xzf "$tarball" -C "$work/unpacked"
unpacked="$work/unpacked"
[ -f "$unpacked/bin/$repo" ] && [ ! -L "$unpacked/bin/$repo" ] \
    || die "bin/$repo is not a regular file in $tarball"
[ -s "$unpacked/share/$repo/CAVEATS" ] || die "share/$repo/CAVEATS is empty"
for name in $repo $names; do
    exe="$unpacked/bin/$name"
    if [ "$name" != "$repo" ]; then
        [ -L "$exe" ] || die "bin/$name is not a symlink in $tarball"
        [ "$(readlink "$exe")" = "$repo" ] \
            || die "bin/$name points at '$(readlink "$exe")', not the relative '$repo'"
    fi
    [ -x "$exe" ] || die "bin/$name is not executable in $tarball"
    "$exe" --help > /dev/null || die "$name --help failed"
    reported="$("$exe" --version)" || die "$name --version failed"
    [ "$reported" = "$name ($crate) $version" ] \
        || die "$name --version says '$reported', expected '$name ($crate) $version'"
done

printf '%s\n' "$tarball"
