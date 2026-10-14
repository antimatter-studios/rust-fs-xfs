### Added

- **An exchange moves reflinked blocks and edits block-map trees (#441).**
  `Filesystem::exchange_range` no longer refuses a file with shared extents
  or one whose extents are in a B+tree. Shared blocks keep their reference
  counts, their reverse-map records overlap one per mapping as the kernel
  keeps them, and a file that takes a shared block takes the reflink flag.
  Each file's new extent list is laid out inline or as a B+tree by the
  kernel's rule, so an exchange can turn either form into the other. The
  kernel replays both cases on the Debian 13 guest and `xfs_repair -n` calls
  the volume clean.
