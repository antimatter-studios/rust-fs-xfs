### Fixed

- **The last name of an inode with attribute blocks can be removed (#495).**
  `unlink_file` of an inode's last name, and a `rename` that replaces one,
  no longer refuse an inode whose attribute fork has blocks of its own: a
  leaf, a node, remote values, or a B+tree mapping them. Each block goes back
  to free space with its reverse-mapping record in the same transaction that
  frees the inode, its buffer is cancelled, and its owners' quota is
  credited. On a volume with parent pointers this is any file with a handful
  of user attributes or two or three long names. The pinned `xfs_repair -n`
  finds the volume clean after an unmount, and the Debian 13 kernel replays
  the change from the log.
