## Interfaces

| `fs.xfs import`: a file, stdin or a tree streamed in through the shared bounded import, staged beside the destination and published by one rename, `--replace` in that rename and `--verify` reading the persisted copy back, and `--preserve=times,links` keeping times, hard links and symbolic links (`--features cli`; v5; sparse files refused) | Supported | Unreleased (#579) | #530, #531 | `cli_import_kernel.rs`, `tests/cli/test-import.sh`, `tests/cli/test-import-preserve.sh` |
