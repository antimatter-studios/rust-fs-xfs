### Added

- **Parent pointers are read (#373).** `Filesystem::parent_pointers` returns
  each name an inode has in a directory, with that directory's inode number
  and generation, on a volume with the parent-pointer feature. A rewrite of a
  short-form attribute fork keeps them, they are never listed or set as
  attributes, and a fork holding one past short form is refused by the
  writer rather than rewritten without it. On the parent-pointer volume the
  reference `mkfs.xfs` builds, every inode's parent pointers are exactly the
  directory entries that name it.
