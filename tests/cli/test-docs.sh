# Every installed tool has its man page and its zsh, bash and fish
# completions where the install prefix keeps them -- share/ beside the bin/
# that PATH found the tool in, the layout of the release tarball and of a
# Homebrew prefix alike. `man -w` finds each page there, and each page names
# every subcommand the tool's --help lists.
source "$(dirname "$0")/lib.sh"

if ! command -v man >/dev/null 2>&1; then
    fail "man is not on PATH, and this file checks the pages the way a person finds them. Install it: \`apt-get install man-db\`."
    finish
fi

for name in $(rust-fs-xfs generate names) rust-fs-xfs; do
    path="$(command -v "$name" 2>/dev/null || true)"
    if [ -z "$path" ]; then
        fail "$name is not on PATH"
        continue
    fi
    share="$(cd "$(dirname "$path")/.." && pwd)/share"
    page="$share/man/man1/$name.1"
    check "$name has a man page at $page" test -s "$page"
    found="$(man -M "$share/man" -w "$name" 2>/dev/null || true)"
    check "man -w $name finds $page (found '$found')" test "$found" = "$page"
    for verb in $("$name" --help | awk '/^Commands:/ { on = 1; next } on && /^  [a-z]/ { print $1 } on && !/^  / { on = 0 }'); do
        [ "$verb" = help ] && continue
        check "$name's man page mentions $verb" grep -q -- "$verb" "$page"
    done
    check "$name has a zsh completion" test -s "$share/zsh/site-functions/_$name"
    check "$name has a bash completion" test -s "$share/bash-completion/completions/$name"
    check "$name has a fish completion" test -s "$share/fish/vendor_completions.d/$name.fish"
done

# The subcommand pages the tool's page points at.
share="$(cd "$(dirname "$(command -v fs.xfs)")/.." && pwd)/share"
for verb in ls read write mkdir get info set resize; do
    check "fs.xfs-$verb has a page of its own" test -s "$share/man/man1/fs.xfs-$verb.1"
done
check "mkfs.xfs has its section-8 page" test -s "$share/man/man8/mkfs.xfs.8"
check "no fsck.xfs page: this crate ships no checker" test ! -e "$share/man/man8/fsck.xfs.8"

finish
