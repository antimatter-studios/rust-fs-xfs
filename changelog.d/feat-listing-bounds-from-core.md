### Changed

- `fs.xfs ls --recursive` uses rust-fs-core 0.4.3's shared arguments, help, defaults and bound-reached errors. The defaults remain depth 64 and 100000 entries, and a reached bound retains the same partial report, error wording and exit status.
