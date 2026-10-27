### Added
- `fs.xfs TARGET write --at BYTES PATH` writes stdin into an existing regular file starting at that byte, in place and without truncating it, and reports `{"path", "at", "bytes", "size"}`. Writing past the end, or into a hole or an unwritten extent, is refused with exit 3, and every refusal leaves the image byte-identical.
