### Added
- `fs.xfs TARGET export [--recursive] PATH DEST` copies a file, or a directory tree, out of the image through rust-fs-core's shared transfer: fixed chunks, a local file published only when complete, no replacing without `--replace`, `--verify`, `--preserve=links,times,sparse`, and every item of a tree reported as completed, skipped, refused or failed.
