## Interfaces

| `fs.xfs rm` and `rmdir`: a name or an empty directory removed through the journalled unlink and directory removal, released clean through the helper and failure path shared with `write` and `mkdir`; refusals decided read-only for the reasons the kernel gives; v4 refused (`--features cli`) | Supported, within the removal shapes the driver frees | Unreleased (#519); shared unmount path Unreleased (#545) | | `cli_rm_kernel.rs`, `tests/cli/test-write.sh`, `tests/scripts/cli-shared-unmount.sh` |
