### Fixed

- **An entry into a group whose inode btree was not read is walked.**
  When a group's AGF, AGI, inode btree or an inode btree record cannot be
  read, an inode in that group reached from a directory is read on its
  own and, when it is in use, walked like any other: its blocks are
  claimed, its entries are followed and its links are counted. The check
  no longer reports such an entry as pointing at an inode in no chunk, a
  root directory there as unallocated, or the blocks it owns in other
  groups as lost.
