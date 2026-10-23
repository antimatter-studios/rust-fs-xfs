## Interfaces

| `fs.xfs` namespace verbs (`rm`, `rmdir`, `mv`, `ln`, `mkdir`, `truncate`) refused by the volume's state (a directory that is not empty, a name already there, a missing target, a part of the path that is not a directory): exit 4, `refused`, as the CLI contract has it | Supported | Unreleased (#573) | #573 | `tests/cli/test-write.sh`, `test-ln.sh`, `test-mv.sh`, `test-truncate.sh`, `test-mkdir.sh`, `cli_rm_kernel.rs`, `cli_mv_kernel.rs` |
