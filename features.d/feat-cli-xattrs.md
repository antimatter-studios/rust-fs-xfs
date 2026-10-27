## Interfaces

| `fs.xfs export` and `import` with `--recursive --preserve=xattrs`: extended attributes in the `user.`, `trusted.` and `security.` namespaces and POSIX ACLs (exported as `system.posix_acl_*`) carried through the shared transfer, a name either side cannot hold refused per item (`--features cli`; not with `--preserve=sparse`) | Supported | Unreleased (#PR) | #532 | `cli_xattr_transfer_kernel.rs`, `tests/cli/test-xattrs.sh` |
