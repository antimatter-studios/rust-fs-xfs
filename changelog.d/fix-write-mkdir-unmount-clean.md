### Fixed

- **`fs.xfs write` and `mkdir` leave the volume clean.** A journalled write,
  a mkdir and a write refused after its file was made release the volume
  through `Filesystem::unmount`, so `get dirty` is false afterwards and the
  next write follows on the same image without Linux mounting it first.
