### Changed

- **A missing path is the `not_found` error kind.** `fs.xfs`'s `ls`, `ls -R`, `stat` and `read` report a path that does not exist with `"kind": "not_found"` beside `error` and `code` in the JSON error, still exit 1, so a script tells it by that field rather than by the message. Built on rust-fs-core 0.4.2, for `CliError::not_found`.
