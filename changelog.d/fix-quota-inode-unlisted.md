### Fixed

- **A quota inode in a group whose inode btree was not read is checked,
  not reported missing.** When a group's AGF, AGI, inode btree or an
  inode btree record cannot be read, a quota inode in that group is read
  on its own and checked like any other, and left unchecked when it is
  not in use, instead of being reported as not a readable allocated
  inode.
