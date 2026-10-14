### Added

- **Volumes with parent pointers are written (#373).** A read-write mount of a
  parent-pointer volume is no longer refused. Creating a file, directory or
  symlink, linking, renaming within or between directories, unlinking and
  removing a directory each add, move or remove the parent pointer their
  change implies, logged in the same transaction as the directory entry. An
  inode whose attribute fork is past short form is refused before anything is
  written. The pinned `xfs_repair -n` calls the volume clean after each kind
  of change.
