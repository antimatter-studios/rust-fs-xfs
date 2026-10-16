### Fixed

- **`fsck.xfs --dry-run`, `-y` and `-p` on a target that cannot be opened
  print an error naming it (#490).** A path that does not exist used to
  print a report on stdout with a plan refused as `repair.not-exclusive`,
  which says another holder has the target, and nothing on stderr. Every
  mode now answers as a check does: exit `8`, an error naming the target on
  stderr, and no report.
