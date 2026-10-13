### Fixed

- **A full volume is ENOSPC, not ENOTSUP (#463).** `Error::NoSpace`,
  mapped to ENOSPC by the C ABI, where an allocation that found no room
  was `UnsupportedFeature` and so ENOTSUP. A data write is `NoSpace` once
  every allocation group has been taken from; an aligned allocation is
  `NoSpace` when the whole volume has fewer free blocks than it wants, and
  stays `UnsupportedFeature` when the space is there but in another group
  or in pieces. A new variant of `Error`, so 0.14.0.
