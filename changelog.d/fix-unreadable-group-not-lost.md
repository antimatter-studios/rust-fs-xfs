### Fixed

- **A group whose AGF or AGI cannot be read is unchecked, not lost.** The
  check no longer reports every block of such a group as `lost`, and no
  longer compares the superblock's inode and free-block counters with
  totals that group never added to; the scan is still reported partial,
  with `ag.agf.unreadable` or `ag.agi.unreadable` for the group.
