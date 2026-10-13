### Changed

- **`fsck.xfs -y` repairs volumes with a reverse-mapping btree (#442).** The
  planner refused any `rmapbt` volume, which `mkfs.xfs` makes by default,
  so on new volumes it repaired nothing. A rule now says whether it keeps
  the reverse map (`Rule::keeps_reverse_map`). Every rule that ships does:
  superblock copies, counters and directory metadata move no block, and
  inode allocation moves one only through the writer, which records it. A
  plan under `rmapbt` is refused only when a rule asked does not.
