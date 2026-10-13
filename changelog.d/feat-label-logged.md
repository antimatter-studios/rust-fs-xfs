### Added

- **A label set after a logged change is logged (#464).**
  `Filesystem::set_label` and `fs_xfs_set_label` used to refuse once the
  mount had logged anything, since a replay of those records would write
  over a label put on the primary superblock in place. The primary is now
  logged with the new label, and the secondaries are still written in
  place, as the kernel writes them. `tests/label_logged_oracle.rs` has the
  kernel replay the label from the log and `xfs_repair -n` find the volume
  clean.
