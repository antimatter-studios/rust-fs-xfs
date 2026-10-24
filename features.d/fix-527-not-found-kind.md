## Interfaces

| `fs.xfs` reading verbs (`ls`, `ls -R`, `stat`, `read`) report a missing path as the `not_found` error kind, exit 1 (`--features cli`) | Supported | Unreleased (#575) | | `tests/cli/test-read.sh`, `tests/cli/test-stat.sh`, `tests/cli/test-ls-recursive.sh` |
