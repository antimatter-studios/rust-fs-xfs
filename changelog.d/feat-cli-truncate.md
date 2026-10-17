### Added
- `fs.xfs TARGET truncate PATH SIZE` changes a regular file's byte length, releases blocks past a shorter end and reads zeros after growth, with read-only refusal checks and a clean release.
