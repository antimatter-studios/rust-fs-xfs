### Fixed

- `fs.xfs rm` and `rmdir` release the volume through the same helper as
  `write` and `mkdir`, so a failed clean unmount reports the same error
  for all four verbs.
