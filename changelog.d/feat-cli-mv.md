### Added

- **`fs.xfs mv SOURCE PATH` gives a name a new one** through the driver's
  journalled `rename`: in place or into another directory, a directory with
  its `..`, and over a name already at PATH as `rename(2)` replaces it, a
  file by a file and an empty directory by a directory. PATH is the new name
  itself, not a directory to move into. It reports
  `{"path": ..., "moved": true}` and releases the volume clean, so
  `get dirty` is `false` afterwards. A missing SOURCE or parent, the root,
  `.` and `..`, a directory moved beneath itself, a file over a directory, a
  directory over anything else or over one with entries are refused for the
  reason the Linux kernel gives, and a v4 volume (exit 3) and a log holding
  records are refused too, all before the image is opened for writing; a
  replacement the driver does not make is refused with its reason (exit 3)
  before anything is written (#520).
