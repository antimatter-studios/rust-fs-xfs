## Interfaces

| `fs.xfs write --at BYTES`: stdin written into an existing file at a byte, in place, refusing a write past the end, a hole or an unwritten extent before the image is opened for writing (`--features cli`) | Supported | Unreleased (#576) | #524 | `cli_pwrite_kernel.rs`, `tests/cli/test-write-at.sh` |
