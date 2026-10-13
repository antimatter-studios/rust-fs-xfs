### Added

- **`fsck.xfs -y` repairs redundant directory metadata (#394).** The
  `DirectoryMetadata` rule rebuilds entry types, entry tags, dot and dot-dot
  entries and hash indexes. Their sources are the validated inodes, the directory
  topology and the entries' own data; names and entry inode numbers are never
  changed. A duplicate name, a dangling entry, uncertain topology, or a preview
  that does not check complete and free of directory findings refuses it. A rule
  may now account for findings that stop the check's walk, and a partial scan
  whose every such finding a rule accounts for is planned rather than refused.

- **`fsck.xfs -y` repairs inode allocation and link counts (#393).** The
  `InodeAllocation` rule sets each inode's allocation bit to whether the inode is
  in use. Both inode trees, the free counts that follow, and every link count are
  derived from checksum-valid inodes and a complete directory traversal. An
  orphan, a directory with two parents, an unused slot still claiming blocks, a
  metadata inode, or any other finding refuses it. The rule checks a view of the
  volume with its changes applied before proposing them.

- **`fsck.xfs -y` counts group and superblock counters again (#392).** The
  `Counters` rule derives each AGF, AGI and superblock counter from the trees it
  summarises, exactly as the checker does, and rewrites only those fields and the
  header's checksum. The AGF counters are free blocks, longest extent and btree
  blocks; the AGI counters are inodes, free inodes and inode btree blocks; the
  superblock's are inodes, free inodes and free blocks. A counter beside any
  other damage refuses the plan, because a summary of trees the check did not
  find sound is not a repair.

- **`fsck.xfs -y` and `-p` repair a damaged secondary superblock (#391).** The
  repair plan of #375 is applied: `fs_xfs::repair::apply` reads every range
  again and writes only if it still holds the bytes the plan saw. The volume is
  then checked again. The first rule, `SuperblockCopies`, rewrites one damaged
  copy from the primary, as `xfs_repair` does. A copy naming another filesystem,
  or more than one damaged copy, refuses the plan, and nothing is written. Exit
  status 1 means corrected; it adds to 4 when errors are left.

### Changed

- **An inode chunk whose free count disagrees with its free mask is
  `inobt.chunk-count` (#393),** not `inobt.record`. The record's first inode and
  masks still say which inodes it covers, so the check reads them and the scan
  stays complete. An inode the inode btree calls free that is in use is walked
  through, rather than hiding the directory tree beneath it.

- **A secondary superblock that fails its checksum is `sb.copy.unreadable` (#391),**
  not `checksum`, and no longer makes the scan partial: nothing is reached
  through a copy, so only the copy went unchecked. `-y` and `-p` are no longer
  refused with 16; they repair, and only asking for them with `-n` or
  `--dry-run` is.

### Fixed

- **An inode chunk may start wherever the superblock aligns it.** A write
  refused a group whose first inode chunk did not start at a multiple of 64
  inodes, but `mkfs.xfs -i sparse=0` puts it at inode 96, block 12 of a
  four-block alignment, and the kernel and `xfs_repair` take it. A chunk now
  has to start on an inode block aligned to `sb_inoalignmt`.
