### Fixed

- `inode::flags::EXTSIZE` selects the extent-size hint bit (`0x0800`) instead of the immutable bit (`0x0008`), matching the flags written by Linux and reported by xfsprogs (#533).
