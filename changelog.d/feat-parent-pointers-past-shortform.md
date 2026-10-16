### Added

- **Parent pointers are kept in attribute forks past short form (#488).** On a
  volume with parent pointers, linking, renaming and unlinking one of several
  names of an inode whose attribute fork is a leaf or a node, or whose new
  pointer no longer fits short form, are no longer refused. The fork is
  rebuilt in one free run, with every user attribute it held and each parent
  pointer filed under the kernel's parent-pointer hash. The new blocks, the
  old blocks' release and the directory entry go in one transaction. The
  pinned `xfs_repair -n` and xfs_db's parent listing agree with every name,
  and the Debian 13 kernel replays the changes from the log.

### Fixed

- **An inode's extent counts are read where its own flag keeps them (#510).**
  On a volume with 64-bit extent counters, an inode without the per-inode
  `NREXT64` flag in `di_flags2`, which includes every inode this driver
  creates, keeps its counts at the narrow offsets, as the kernel and xfs_db
  read and write them. The reader had taken the volume's feature bit instead,
  so such a directory read as having no blocks once it left its inode, and
  such a file's attribute fork past short form read as empty. Attribute
  writes now put the attribute fork's count where the same flag says, too.
