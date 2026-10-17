### Added

- **A value stored in blocks of its own is kept beside parent pointers
  (#501).** On a volume with parent pointers, linking, renaming and
  unlinking one of several names of an inode whose attribute fork holds a
  user, trusted or security value too large for its leaf are no longer
  refused. The value is rebuilt with the fork and, since it is not logged,
  written before the transaction that maps it, into a run that was free
  before that transaction began: never into a block the same change gives
  back, such as a replaced file's data, which stays in use until the
  transaction commits. The pinned `xfs_repair -n` calls the volume clean,
  and the Debian 13 kernel replays the changes from the log and reads the
  value back unchanged.
