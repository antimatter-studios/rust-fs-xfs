## Interfaces

| `fs.xfs mkdir --parents` (`-p`): missing directory chains through one writable mount and clean release, existing chains succeeding read-only, and path obstructions refused before any create | Supported, within the driver's directory creation shapes | Unreleased (#551) | #522 | `cli_mkdir_kernel.rs`, `tests/cli/test-mkdir.sh` |
