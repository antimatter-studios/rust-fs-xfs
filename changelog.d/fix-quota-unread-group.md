### Fixed

- **Quota usage is not compared when a group's inodes were not all listed.**
  When a group's AGF, AGI, inode btree or an inode btree record cannot be
  read, the inodes it holds are missing from the quota usage the check
  adds up. The check now leaves quota usage unchecked in that case instead
  of reporting records whose counts include those inodes as wrong.
