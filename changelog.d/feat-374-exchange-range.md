### Added

- **Two files exchange ranges of their data (#374).**
  `Filesystem::exchange_range(a, a_offset, b, b_offset, length)` swaps
  whole-block ranges of two regular files by exchanging the blocks that map
  them: extent lists, block counts, reverse-map records and quota move in one
  checkpoint, and no data is copied. A volume with the exchange-range feature
  is now written; one with parent pointers is still refused. The kernel test
  runs on a second guest, Debian 13 (kernel 6.12), through `chore
  test:kernel-new` and its own CI job, because Debian 12's 6.1 cannot mount
  such a volume.
