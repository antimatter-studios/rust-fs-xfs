### Added

- **`fs.xfs rm PATH` and `fs.xfs rmdir PATH` remove a name** through the
  driver's journalled `unlink_file` and `remove_directory`. `rm` takes away
  a name that is not a directory (a symlink, never what it points at), and
  the file goes with its last name; `rmdir` takes away an empty directory.
  Both report `{"path": ..., "removed": true}` and release the volume clean,
  so `get dirty` is `false` afterwards. A missing path, a directory given to
  `rm`, anything but a directory given to `rmdir`, a directory with entries,
  `.`, `..` and the root are refused for the reason the Linux kernel gives,
  and a v4 volume (exit 3) and a log holding records are refused too, all
  before the image is opened for writing (#519).
