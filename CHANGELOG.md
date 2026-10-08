# Changelog

Notable changes to `rust-fs-xfs` (published as `am-fs-xfs` until its last version), newest first. This is a `0.x` crate, so the
**minor** is the compatibility boundary: a minor bump may break API, a patch
never does.

## [Unreleased]

### Fixed

- **Quota block usage and hard limits use Linux's filesystem-block units
  (#396).** Directory growth, writes, and truncation account actual filesystem
  blocks for user, group, and project quotas. A write exactly at a hard limit
  succeeds; an over-limit write is refused before data or metadata changes.
- **A job that stops before its first tier shows one red, not two.** Each
  `Keep the tier logs` upload in CI now waits for the job's first `chore test*`
  step to have run, so a failed `chore lint` is no longer joined by an upload
  that found no log. A tier that ran and wrote no log still fails the upload,
  and `tests/ci_profile.rs` holds every job to that.
- **A transient HTTP 5xx from the chore release download no longer fails a CI
  job.** `scripts/ci-install-chore.sh` retries both downloads up to five times
  on any error; the checksum check still guards what was fetched.

## [0.12.1] — 2026-10-07

A tooling release: the library and its C ABI are unchanged.

### Fixed

- **The tools are released again.** 0.12.0's GitHub release has no tarballs:
  its crate was published by hand, CI's repackaged `.crate` did not match the
  published one, and the release stopped at the attestation check. 0.12.1 is
  published by CI through crates.io trusted publishing, and rust-fs-core
  0.3.7's release workflow packages the darwin-arm64 and linux-x86_64 tarballs
  with their attestations.

### Changed

- **The family's scripts run in place from rust-fs-core, and this repository
  keeps no copy.** `scripts/core.sh` and `scripts/tier.sh` are gone; CI and
  chores run `../rust-fs-core/scripts/NAME.sh` at the pinned version
  (rust-fs-core 0.3.3, #212).
- **A release's notes are its CHANGELOG section**, and a tag the CHANGELOG
  does not describe stops before anything is published (rust-fs-core#209).
- **rust-fs-core 0.3.7.** Cargo, both lockfiles and every workflow that
  clones the rust-fs-core sibling move to it; its release workflow is the one
  that packages the tools.

## [0.12.0] — 2026-10-06

### Changed

- **Published as `rust-fs-xfs`, the repository's name.** The crate was `am-fs-xfs`
  until its last version, which stays on crates.io pointing here. A
  dependent changes one line in `Cargo.toml`; the import (`fs_xfs`) and the C symbols are unchanged.
- **Depends on `rust-fs-core` 0.3.0**, the same library under its new name.

## [0.11.0] — 2026-10-06

**Breaking:** `Error` gains the variant `InvalidGeometry`, so this release is
0.11.0. A `match` over `Error` without a wildcard arm needs one more arm.

### Renamed

- **The last version published as `am-fs-xfs`.** The crate is renamed to
  `rust-fs-xfs`, the repository's name; every later version is published under
  that name only, starting at 0.12.0. The description and the README say where
  the crate went. The import is unchanged: `use fs_xfs::...` keeps working.

### Added

- **`fsck.xfs`, and `fs_xfs::check` under it (#339).** Checks a volume
  without changing it: secondary superblocks, each allocation group's
  headers against its btrees, one owner for every block, allocated inodes,
  the directory tree and link counts, and the superblock's counters. Exit 0
  clean, 4 problems found, 8 could not check, 16 usage (a repair request is
  refused). Agrees with `xfs_repair -n` on twelve clean fixtures and ten
  kinds of damage.
- **`fs.xfs set label` and `Filesystem::set_label` (#341).** The label is
  written into the superblock of every allocation group, each with a fresh
  CRC on a v5 volume, secondaries first and the primary last, as the
  reference tool writes it. A label over 12 bytes, and a volume whose log
  is not clean, are refused with the image untouched.
- **`mkfs.xfs`, and `fs_xfs::mkfs` under it (#338).** A v5 filesystem with
  the standard formatter's default features, laid out exactly as it lays
  one out: on a device of the same size with the same UUID, every
  allocation group's headers and btree roots are byte-identical to its
  output (`tests/mkfs_layout.rs`), and `xfs_repair -n` and the kernel accept
  the result (`tests/cli_mkfs_kernel.rs`). Block sizes of 1, 2 and 4 KiB,
  devices of 300 MiB and up, `-b size=`, `-d agcount=`, `-L`, `-m uuid=`,
  `-f`, `-q`, `-N`; every other option is refused by name.
- **`Error::InvalidGeometry`**, for a format that cannot be made: a device
  too small, a block size or group count out of range, a label too long.
  `EINVAL` through the C ABI.

### Fixed

- **A free checks the reference-count records before it edits the tree on
  their word (#92).** The tree's blocks were verified, but the records
  inside them were used as they stood: an empty record, a shared record
  with fewer than two owners, a staging record with other than one, a
  record on the group's headers or past its end, or two out of order or
  overlapping were all accepted, and the truncate that consulted them
  rewrote the tree in the same transaction. Two records out of order made
  `release` splice one index and then index past the end of the list. Each
  is now refused as `xfs_refcount_check_irec` and the tree's key order
  refuse it, before anything is built.
- **An edit keeps staging records after shared ones.** `merge_adjacent`
  sorted by block and then by the copy-on-write flag, which put a staging
  record among the shared ones; the tree's key carries the flag in its top
  bit, so the kernel keeps every staging record after every shared one.
- **A write checks the group-tree records it consumes, as the kernel does.**
  A free-list entry that is `NULLAGBLOCK`, past the group or on its headers
  is refused by `Agfl::take` instead of becoming a tree block written there.
  The by-length free-space tree's records are compared with the by-block
  tree's, and a group where they differ is refused instead of having both
  trees rewritten from one of them. An inode chunk that does not start on a
  chunk boundary inside its group, past its headers, is refused before a
  create builds a file on it (#314).

### Changed

- **The release tarball is packaged, attested and attached by
  rust-fs-core's release-cli workflow, not by a copy here.** `release.yml`
  calls `antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml`,
  pinned by commit SHA at v0.2.23, in place of its own `package-cli` matrix
  and attest-and-attach job, and `scripts/package-cli.sh` and its shell test
  are gone: the pull-request check runs core's script through
  `scripts/core.sh package-cli`. What the tarball ships is declared in
  `Cargo.toml` under `[package.metadata.package-cli]`; its name and layout
  are unchanged. The tarballs' attestations are now signed by core's
  workflow, so `gh attestation verify --signer-workflow` names
  `antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml` for
  them; the `.crate`'s is unchanged. am-fs-core moves to 0.2.23, the first
  release with the workflow (#326).

- **A test run gives the harness VM and its machine-wide slot back when it
  ends, however it was started.** The oracle and kernel tests boot the VM
  from their own process, and stopping it was left to chore's `after_all`
  reaper, which runs only inside a chore invocation of this repository: a
  run made any other way (`scripts/with-test-temp.sh` by hand, `scripts/tier.sh`) exited
  with the VM idle and the slot held, and every other repository's VM work
  queued behind it until the guest's idle deadline. `scripts/with-test-temp.sh` now runs
  cargo through the harness's `vm.sh session`, which brings the VM down
  and releases the slot when the run ends — passed, failed or killed —
  leaves a VM held with `chore vm:up` alone, and runs cargo as it is in the
  guest and on a host that cannot run the VM. The harness moves to v0.3.0,
  the release that provides it (fs-linux-test-harness#37, #36).
- **An extent outside its allocation group is refused, not used.** A data
  fork's extents were taken as they stood. An XFS block number is packed,
  and its group part rounds up to a power of two, so an extent past its
  group's length addressed the next group's superblock, AGF, AGI and free
  list. `write_at` wrote the file's bytes there, and `truncate_to_zero`
  journalled a free of them. Every extent is now checked as the kernel's
  `xfs_verify_fsbext` checks it: the group exists, the extent starts past
  the group's headers, and it ends inside the group (#92).
- **An inode-chunk record whose counts disagree with its masks is refused.**
  A create chose an inode by the record's free mask and then decremented
  the record's `u8` free count. A count above the mask's was written back
  into both inode trees, and a count of zero beside free bits panicked in
  a debug build and wrapped to 255 in release. Every record is now checked
  as the kernel's `xfs_inobt_check_irec` checks it, and `take` cannot
  decrement through zero. `Trees::open` also no longer replaces a record
  it refuses with an all-zero chunk, which laying the trees out again
  would have written over the real one (#314).
- **A create no longer builds a file on an inode slot it has not
  verified.** The free inode's slot was read straight off the device and
  its magic, version, number and UUID kept, with no check of its checksum,
  its identity or that it was free at all. A stale or misread free bit
  therefore handed out a live file's inode and journalled a new file over
  it. The slot is now checked as the kernel checks it, `di_mode == 0`
  included, and a create refuses one that fails (#92).
- **An inode's version is checked against the filesystem's.** `Inode::parse`
  accepted versions 1, 2 and 3 on any filesystem, and checked the CRC,
  `di_ino` and `di_uuid` only when the record itself claimed version 3. On
  a v5 filesystem one flipped bit (3 to 2) switched those checks off and
  moved the data fork 76 bytes into the v3 core, whose checksum and LSN
  were then decoded as extents for a write to act on. A v5 filesystem's
  inodes must now be version 3, and a v4 filesystem's version 1 or 2, as
  the kernel's `xfs_dinode_good_version` requires (#92).
- **An allocation checks the free-space records it takes from.** The
  by-block tree's blocks were verified, but its records were handed out as
  they stood. A record naming the group's headers gave a new file block 0,
  the primary superblock, and `write_into_empty_file` wrote the file's
  bytes there. A record past the group's end or overlapping its neighbour
  was taken the same way. Every record is now checked when the group is
  opened, as the kernel's `xfs_alloc_check_irec` checks it, and in
  ascending order (#314).

## [0.10.0] — 2026-10-01

### Breaking

- **`Error` gains `RealtimeDeviceAbsent { ino }`.** Reading a realtime
  file's data on a mount that was not given the realtime device returns it,
  where it used to be `UnsupportedFeature`, and the C ABI reports it as
  ENXIO rather than ENOTSUP. `Error` is not `#[non_exhaustive]`, so an
  exhaustive `match` on it needs a new arm (#98).

### Added

- The tools write their own man pages and shell completions (`rust-fs-xfs
  generate man|completions SHARE`): a section-1 page per name and per
  `fs.xfs` subcommand, and zsh, bash and fish completions, which
  `chore cli:install` stages and the release tarball carries under
  `share/`. `clap_complete` and `clap_mangen` (MIT/Apache-2.0) join clap
  behind the `cli` feature (#271).
- `Filesystem::mount_with_realtime(data, realtime)` reads the files on a
  volume's realtime section from its realtime device, byte for byte as the
  kernel reads them; `mount` alone still mounts such a volume and reads
  everything else (#98).
- The C ABI reaches it: `fs_xfs_mount_with_realtime(device_path,
  realtime_path)` and `fs_xfs_mount_with_realtime_callbacks(cfg,
  realtime)`, read-only. A test holds `include/fs_xfs.h` to declare exactly
  the functions the library exports (#291).

### Fixed

- `create_file` and `create_directory` write the kind's type bits into
  `di_mode` when given a mode of permissions only, where `0o644` used to
  make an inode of no type; a mode whose type bits name another kind is
  refused. A new inode's atime, mtime, ctime and crtime are the time of the
  create, where they were the free slot's own, which is 1970 on a fresh
  volume (#276).
- A v5 directory block's CRC, block number, UUID and owner are verified
  when it is read — by a listing, a lookup and a create — so a damaged or
  foreign block is refused as the kernel refuses it. Before, nothing
  checked them, and `add_to_block_form` would rebuild such a block as a
  checksum-valid block of the directory it was adding to (#287).

- `fs_xfs_last_errno` is 0 after a successful call. Every entry point
  resets it on entry, so a clean end of directory reads as errno 0 even
  on a thread where an earlier lookup failed; before, the errno of the
  thread's last failure stayed until the next one, and a caller following
  the header read every later end of directory as a failure. The message
  is kept until the next failure, as the header already promised (#281).
- `create_file`, `create_directory`, `unlink_file` and
  `rename_in_directory` stamp the directory whose entries they change with
  the time of the change, mtime and ctime both, and a rename moves the
  renamed inode's ctime, as the kernel's `xfs_create`, `xfs_remove` and
  `xfs_rename` do. They left them as they were, so a tool deciding from a
  directory's mtime whether to rescan it did not see the change. The
  kernel oracles read the times back after replaying the record (#279).
- **A create in a directory with a default ACL is refused.** The kernel
  gives an inode made there an access ACL from the directory's
  `SGI_ACL_DEFAULT` (and a new directory a copy of it); this driver writes
  no attributes, so `create_file` and `create_directory` now refuse such a
  parent by name, before anything is logged, rather than make an inode
  without the ACL its directory promises. An oracle suite has the kernel
  set the ACLs, shows its own create there inheriting one, and replays the
  creates the driver did make elsewhere under `xfs_repair -n` (#284).

- **A POSIX ACL is listed as root lists it through the kernel.**
  `list_xattrs` and `get_xattr` report `system.posix_acl_access` and
  `system.posix_acl_default` in the VFS's `posix_acl_xattr_header` format,
  translated from the stored `struct xfs_acl` as `fs/xfs/xfs_acl.c` does.
  Before this, a file with an ACL answered `None` to the portable name. The
  stored `trusted.SGI_ACL_FILE` / `trusted.SGI_ACL_DEFAULT` stay listed
  right after it with their stored bytes, as the kernel lists them to root.
  A stored ACL the kernel would refuse (a length that disagrees with its
  count, an undefined tag, more than 25 entries on v4) fails the listing as
  corrupt. The listing is held, byte for byte, to `getfattr -d -m - -e hex`
  run as root in the guest, on v5 and v4 (#285).

## [0.9.0] — 2026-09-30

### Breaking

- **In-image paths cross the C ABI as bytes, not UTF-8 (#269).**
  `fs_xfs_stat`, `_dir_open`, `_read_file`, `_readlink`, `_write_file`,
  `_truncate` and `_set_attributes` read their `const char *` as the bytes
  up to the NUL and compare them byte for byte against the names in the
  image; they no longer decode it. `fs_xfs_dir_next` already reported names
  as raw bytes, so the library handed out names it then refused with "not
  valid UTF-8". A path naming no file is now reported as missing
  (`ENOENT`), not as a bad argument, and the write entry points name the
  same files the read ones do.

  **Source-compatible for every caller passing UTF-8**, because UTF-8 is a
  byte string too. `open_bytes`, `lookup_path_bytes`, `read_path_bytes` and
  `list_path_bytes` carry the resolution and the `&str` forms wrap them, so
  the Rust API is unchanged. `fs_xfs_mount` and `fs_xfs_mount_rw` keep
  their UTF-8 decode: their argument is a path on the host filesystem, not
  an in-image name.

### Added

- Releases carry a build-provenance attestation: the published `.crate` is
  attached to the GitHub release for its tag, checked first against the
  crates.io checksum, and verifiable with `gh attestation verify` (see the
  README, "Verifying a release").
- **`fs.xfs`, a command-line tool over the library**, and `rust-fs-xfs
  doctor`. One multi-call binary named for the repository, behind a new
  `cli` feature so the static library gains no dependency; `fs.xfs` is a
  symlink to it. `get`/`info` report the superblock as JSON (`--text` for
  people), `set label` and `resize` answer `not implemented` with exit
  status 3, every failure is a structured error on stderr, and `--offset`
  reaches a filesystem inside a whole-disk image. Releases attach an
  attested install-prefix tarball per platform (#271).
- A `cli` test tier (`chore cli:install`, `chore test:cli`) tests the tools
  as installed, doctor first, against a new `cli` fixture set: a v5 and a v4
  image made by `mkfs.xfs -L` and filled by the kernel. `get` is held to
  `xfs_db` and `xfs_info` in the oracle tier (#271).
- `fs.xfs ls` and `fs.xfs read`, on v4 and v5: typed JSON entries (name,
  type, size, mode, mtime, inode, a symlink's target) and a file's raw bytes
  on stdout or `-o FILE`. The tier holds every listing and every file's
  SHA-256 to the manifest the kernel wrote, and a copy with an inode that
  fails its CRC or an AGI with a bad magic is refused with a structured
  error and nothing on stdout -- a refusal `xfs_repair -n` in the guest
  agrees with (#271).
- `fs.xfs write` and `fs.xfs mkdir`, within the driver's measured shapes: a
  new file in one extent (v5), an empty file given contents, a same-length
  overwrite in place (v4 and v5), and a directory (v5). Every other shape is
  refused with the driver's own reason and exit status 3, and a write that
  is refused after it had created its file unlinks it again. The kernel in
  the guest replays every write, reads each file back by SHA-256, and
  `xfs_repair -n` accepts the result (#271).

### Changed

- `Filesystem::mount` finds the log's newest record from its cycle numbers,
  as the kernel's `xlog_find_head` does, rather than reading the whole ring,
  and reads it once rather than twice: a clean mount of a volume with a
  64 MiB log read 128 MiB and now reads 80 KiB. A log the search cannot
  settle falls back to the whole-ring scan, with the same answer (#251).

## [0.8.0] — 2026-09-27

### Breaking

- **`fs_xfs_readlink` follows the readlink contract every driver in the
  family now shares.** Success returns the target's length excluding the
  NUL, as `readlink(2)` does, with the target and a NUL written into
  `buf`; a caller still testing `== 0` for success must test `>= 0`. A
  buffer smaller than length + 1 is -1 with `ERANGE`, a message naming the
  size needed, and nothing written — never a truncated target. What
  changed for a caller of 0.7.0: NULL `fs`, `path` or `buf` is now
  `EINVAL` (was `EIO`, or `ENOENT` for `path`); a zero `bufsize` is
  `ERANGE` like any other buffer too small (was `EIO`); and a path that
  is not a symlink is `EINVAL`, as `readlink(2)` has it (was `EISDIR`).
  The data fixtures gain a remote (block-stored) symlink, and
  `tests/capi.rs` checks every fixture's links against the kernel's own
  `readlink` (#259).

### Added

- **The parsers are fuzzed, on two tiers.** Nothing in this crate had a
  fuzz target, and the 2026-09-06 hardening wave — declared sizes used as
  allocation sizes, walks with no visit budget, arithmetic that wrapped
  with `overflow-checks` off — was a list of what a fuzzer finds in
  minutes and a person finds by reading for an afternoon. Both halves
  read one committed corpus of real structure blocks, cut out of an image
  `mkfs.xfs` wrote and rebuildable in two seconds by
  `scripts/make-fuzz-corpus.sh`. `fuzz/` holds thirteen `cargo-fuzz`
  targets covering the superblock, the AG headers and free list, inodes
  in each format, every directory block form, the btree leaves the
  free-space, rmap and refcount trees share, the block-map btree and the
  extent list; they run nightly on a bounded budget and upload whatever
  they find. `tests/fuzz_decoders.rs` is the gate: it replays the corpus
  verbatim and applies deterministic seeded mutations to it, 43,000 cases
  in under half a second on the stable toolchain, so anything the fuzzer
  finds stays fixed once its input is committed. It fails on a hang as
  well as a panic, naming the target, seed and case; it refuses a case
  count below a floor, so a suite that stopped generating work cannot
  pass by doing nothing; and it refuses a `cargo-fuzz` target that has no
  counterpart in the gate, so the two tiers cannot drift (#96).

- **A checkpoint larger than one in-core log buffer is written as several
  records.** One operation's record had to fit a single buffer — 32 KiB on
  an ordinary log — and anything larger was refused, which an ordinary
  operation reaches: truncating a file interleaved with another leaves
  three thousand free runs that do not merge, the group's trees are laid
  out again over dozens of blocks, and every one of them is logged. The
  operations are divided at operation boundaries and written as a sequence
  of records; nothing marks the split, because the transaction id ties them
  together, the first record holds the `START` and the last the `COMMIT`.
  An operation that alone exceeds a record is still refused, and says which
  one it was. The whole checkpoint is placed before any of it is written,
  so a wrap cannot leave recovery to start in the middle of it (#216).

- **A volume with a dirty log mounts, replaying into memory.** A crash, a
  panic or a yanked cable leaves the log holding committed transactions the
  metadata has never been given, and such a volume was refused outright —
  honest, and it meant the ordinary state after a crash was one this driver
  could not open at all, where the kernel simply replays and mounts. A
  read-only mount now reads the records from the tail to the head and
  applies them: buffer items write their logged chunks, inode items their
  core and forks, an icreate item initialises the chunk of inodes the record
  did not carry, and a block that was cancelled takes nothing from before it
  changed hands. The volume itself is **not written to** — the replay is
  held in memory over an untouched device, which is what recovering data
  from a disk one may not touch needs — and `Filesystem::was_replayed` says
  which kind of mount this is. A read-write mount still refuses a dirty log.
  Intent items are counted rather than finished, which leaves space
  accounting unsettled and nothing a directory walk or a file read can see
  (#90).

- **A directory takes entries after it has outgrown the inode.** A
  short-form directory that would not hold one more name was moved into a
  block of its own, and the next entry was then refused: "has outgrown the
  inode, so adding an entry rewrites a directory block rather than the
  inode's own fork". A directory therefore held about two dozen names and no
  more. A create into a directory already in block form now lays that block
  out again with the entry in it, through the same `dir_block::build` the
  conversion uses, and logs the block. A 4 KiB block holds about 124 names
  of 10 characters; past that the create is refused naming leaf form, which
  is not implemented (#215).
- **A mount keeps going, in bounded memory.** Every buffer a record carried
  was held for as long as the mount ran and the log was never reused, so the
  memory grew with the run and the 32,751st operation was refused with
  "only 2 remain before the log wraps". `Filesystem::sync` now writes those
  buffers where they belong, flushes and lets go of them — the push an XFS
  mount makes through the AIL — and it runs when 16 MiB is held, when a
  record will not fit in what is left of the ring, and whenever a caller
  asks. The ring is then started again from its beginning in the next cycle,
  with the blocks left at its end filled by an empty record, because a
  reader walks the cycle number in every block and a gap reads as a corrupt
  header. `log_wraps` and `dirty_bytes` report both. The head is remembered
  between records rather than found by scanning the ring, which was 52 ms an
  operation (#89).
- **A mount writes as many journalled operations as it likes.** A record
  changes nothing on disk, so the second operation of a mount read the state
  the first started from — two creates handed out one inode — and the mount
  refused it rather than write it. Every buffer a record carries now goes
  into an overlay that a writable mount reads through, so each operation is
  built on what its predecessors logged, which is what a replay arrives at.
  What goes in is what recovery writes: buffer images with the checksum
  recovery computes, inode cores with their fork and CRC, and every inode of
  a chunk an `icreate` names. `h_tail_lsn` names the mount's first
  outstanding record rather than the record itself, because a tail past a
  record recovery still needs loses a committed transaction. In-place writes
  are still refused once anything has been logged (#89).
- **Volumes with parent pointers or exchange-range can be read.** Both
  incompat bits are accepted by a read-only mount. `mkfs.xfs -n parent=1`
  sets both, and `-i exchange=1` sets exchange-range alone. Parent pointers
  are attributes in their own namespace, which `list_xattrs` already leaves
  out. `mount_rw` refuses either bit, saying the volume can be read but not
  written, because no write here maintains parent pointers.
  `tests/parent_exchrange_oracle.rs` builds both kinds of volume with a
  pinned xfsprogs 6.13 (`scripts/build-xfsprogs.sh`), which CI builds and
  caches (#99).
- **Extended attributes can be read.** `Filesystem::list_xattrs` and
  `get_xattr` read an inode's attribute fork in every shape: short form
  inline in the inode, a single leaf block, a node B-tree over chained
  leaves, and remote values over several blocks (each block's v5 header
  stripped). Names come back with their `user.`, `trusted.` or `security.`
  prefix; entries still marked incomplete are not returned.

### Changed

- **A lookup goes through the directory's hash index.** `Filesystem::lookup`
  listed the whole directory to find one name, reading every data block of
  it for every path component. Block-, leaf- and node-form directories are
  now searched by name hash (the block's tail index, the leaf block, or the
  node B-tree down to a leaf), and only the data blocks the matching
  records point at are read: one lookup in a 20,000-name directory makes 6
  device reads where the listing made 122. Short-form directories, which
  live in the inode, are scanned as before.
- **BREAKING (C ABI).** `fs_xfs_set_attributes` and `fs_xfs_truncate`
  take a new sentinel for timestamps: `FS_XFS_LEAVE_TIME`
  (`INT64_MIN`), not `FS_XFS_LEAVE` (`-1`). A caller that passed `-1`
  to leave `atime_sec` or `mtime_sec` alone will now SET that timestamp
  to one second before the epoch. There is no version of this fix that
  is not a break: `-1` is a real date, 1969-12-31T23:59:59Z, and making
  it reachable is the point. Every negative timestamp used to mean
  "leave this alone", so no date before 1970 could be set at all — the
  call returned 0 and the field did not move. `mode`, `uid` and `gid`
  keep `FS_XFS_LEAVE`, which is the `chown(2)` convention and correct
  for them.
  Callers should replace `FS_XFS_LEAVE` with `FS_XFS_LEAVE_TIME` in the
  `atime_sec` and `mtime_sec` positions, and in `fs_xfs_truncate`'s
  `mtime_sec`. Dates earlier than 1901-12-13 are accepted by the ABI
  and clamped by the on-disk encoding, which is unchanged.

### Fixed

- **A short-form directory's inode-number width comes from its inode
  numbers.** The width was taken from the header that was parsed, so a
  directory made while the numbers were small stayed four bytes wide
  however large the next one was, and the top half of it was dropped:
  measured, this driver created inode 4294967437 in the third allocation
  group of a volume with terabyte groups and the kernel read the name back
  as inode **141**. An entry naming a different, existing inode, with
  nothing reported. The count is recomputed from what is about to be
  written, as `xfs_dir2_sf_check` computes it — every number past
  `XFS_DIR2_MAX_SHORT_INUM`, the parent included — which makes an addition
  the conversion the kernel calls `xfs_dir2_sf_toino8`, and narrows the
  directory again when the last wide entry goes (#235).

- **A file whose extents live in a B+tree can be truncated.** Once a file has
  more extents than its inode holds, its map moves into a B+tree, and
  freeing such a file was refused: the tree's own blocks belong to the inode
  and would have been left allocated. `truncate_to_zero` now walks the fork
  with `bmbt::walk_with_blocks`, frees the data and the tree's blocks
  together — the latter recorded in the reverse map as `OFF_BMBT_BLOCK`, as
  the kernel records them — and puts the fork back as an empty extent list
  (#222).
- **A new inode chunk starts on the inode alignment.** A create that needed
  a new chunk took its blocks from the first free run long enough, wherever
  that run started. The kernel finds a chunk's inodes by masking their block
  down to `sb_inoalignmt`, so on 1 KiB blocks, where that is 32, a chunk at
  block 91 had its inodes replayed over the file data at block 80. The
  kernel refused the log (`xlog_recover_items_pass2`, error 117), and the
  volume would not mount. Chunks are now taken from an aligned start, by the
  kernel's rule in `xfs_ialloc_cluster_alignment`. On 4 KiB blocks the
  alignment is 8, and the chunk the tests used happened to start on it.
  `tests/inode_chunk_alignment.rs` runs creates on 1 KiB blocks, with the
  kernel replaying every step, until a second chunk is needed.
- **A group's free list is refilled.** Every block a group's B+trees grow
  into comes off its free list (AGFL), and the driver only ever took from
  it. On rmapbt, where a 1 KiB leaf holds 40 records, a few splits emptied
  the list, and from then on every write that needed a tree block was
  refused with `…free list is empty, and refilling it is not implemented`.
  Before a group's trees are laid out again, the list is now topped up out
  of free space to the kernel's `xfs_alloc_min_freelist`: twice the height
  of each free-space tree and of the reverse map. Refill blocks are recorded
  in the reverse map as `OWN_AG`. `tests/agfl_refill.rs` has the driver write
  500 one-block files on rmapbt, with the kernel replaying each; before the
  fix, write 432 was refused (#197).
- **A created file no longer inherits the flags of the file its inode last
  held.** `unlink_file` left `di_flags`, `di_flags2` and the attribute fork
  in the inode it freed, and `create_file` read them back, so a new file
  could come out immutable, append-only, real-time or reflinked. A freed
  inode is now reset the way the kernel's `xfs_ifree` resets it, and a
  created one starts with no flags beyond `BIGTIME` and `NREXT64` (#189).
- **In-place writes are refused once a mount has logged a change.** A
  logged operation writes only its record, so the disk is out of date until
  replay. `write_at`, `set_attributes` and `truncate` read that stale disk:
  a write after `truncate_to_zero` reported success into blocks replay then
  frees, and an attribute change was put back by replay. They now refuse
  after the mount's checkpoint, as a second logged operation already did
  (#186).
- **A rename or create refuses a name no directory entry can hold.**
  `rename_in_directory` checked only the new name's length, and logged
  names containing `/` or NUL, and `.` and `..`. `create_file` let NUL
  through, and a name longer than the one-byte length it is stored in.
  Both now apply the kernel's `xfs_dir2_namecheck` rule, before claiming
  the mount's checkpoint (#192).

## [0.7.0] — 2026-09-06

### Fixed

- Every write path works in a group whose B+trees are more than one
  block deep. There were 27 places that refused one, and a 4 KiB root
  holds 505 free-space records or 252 inode chunks, so any filesystem
  with real fragmentation or more than sixteen thousand inodes hit them.
- The reverse-mapping tree is read as the interval tree it is: a node
  entry carries the lowest key beneath it and the highest, so its
  pointer array starts twice as far into the block. Reading it as one
  key per entry took a pointer out of the middle of the key array, which
  came back as block zero -- the superblock. Only reachable at two
  levels or more.
- One operation allocates once from a group however many times it takes
  from it. A create that needed both a new inode chunk and a directory
  block read the group twice and handed out the same run twice.
- Freeing part of a shared extent splits the reference-count record
  rather than refusing, and records that adjoin and say the same thing
  are merged -- xfs_repair reads three records where one belongs as a
  reference count that is simply wrong.

### Added

- `ag_btree`, the one descent and one layout for a group's four
  short-form trees, and `agfl`, the group's free list.

## [0.6.0] — 2026-09-04

Minor rather than patch: `File` and the three constructors that return
it are new public API, and for a `0.x` crate the minor is the
compatibility boundary. Nothing existing changed — every low-level
`(&Inode, &[u8])` call keeps its signature.

### Added

- **`File`, and `Filesystem::open` / `open_ino` / `root`.** XFS threads
  the raw inode fork through every read, because an inode keeps its
  extents — and when small enough its data or directory entries —
  inside that fork. So the low-level calls take `(&Inode, &[u8])`, and a
  caller had to carry both values and remember they belong together.
  `File` carries them.

  It also removes a read. `lookup_path` walks the tree holding
  `(inode, raw)` at every component and returns **only the inode**, so
  anyone who then read the file fetched the last inode a second time.
  `read_path` and `list_path` did exactly that, in this crate. They
  delegate to `open` now.

  Measured, not asserted — `tests/file_handle_oracle.rs` wraps the
  device in a counter and compares the routes on a real `mkfs.xfs`
  image:

      /small.txt: handle 4 reads / 1548 bytes, low-level 5 reads / 2060 bytes

  `File` is a **snapshot**: nothing invalidates it, so one held across a
  write to the same inode is stale. Safe for open-read-drop within an
  operation; re-open otherwise, which is one inode read.

### Changed

- `lookup_path` is retained and now delegates to `open`, with docs
  saying why `open` is the one to reach for.


## [0.5.2] — 2026-09-04

### Added

- **A superblock can be written back, and is proved against `mkfs.xfs`.** Every
  field is modelled, so one can be built from nothing rather than only edited
  in place.

### Fixed

- **The block-map tree's block addresses are checked**, as the other two trees
  already were. An unchecked address is a read at an arbitrary offset.

### Changed

- Truncate now says which of its two paths journals and which frees the blocks.
  They are different operations with different crash behaviour and the names
  did not distinguish them.

## [0.5.1] — 2026-08-29

### Added

- **Directory writes: make a directory, build a block-form directory byte for
  byte as the kernel builds it, and convert a directory to block form** — the
  last of the measured transaction shapes.
- The build and the tests run through `chore`.

### Fixed

- **The mount's one checkpoint is spent on writes, not on refusals.** A refused
  operation was consuming it, so a later legitimate write had none left.
- The VM lock no longer skips tests quietly — a skipped test that looks like a
  passing one is worse than a failure.
- Test VMs are brought down when a fixture build finishes, teardown confirms
  the machine is actually down rather than assuming it, and a VM leaked by
  `lifecycle: after_all` is reaped.

### Changed

- Pinned toolchain moves to 1.95.0, in lockstep with the rest of the family.
- The CI lint gate can be run locally, and it is the same gate everywhere.

## [0.5.0] — 2026-08-26

### Added

- **The write path, exposed through the C ABI**: overwrite file data in place,
  change an inode's timestamps, permissions and ownership, and shorten a file.
  The inode-update path is shared rather than repeated per operation.

## [0.4.0] — 2026-08-25

### Added

- **B+tree-format data forks** are read, not just extent-format ones.

### Fixed

- **A dirty log is decided by reading the log**, rather than by a heuristic
  that could call a clean filesystem dirty or the reverse.

## [0.3.0] — 2026-08-25

### Added

- `fs_core` mounting, and a C ABI aligned with the sibling drivers so a host
  binds all of them the same way.

### Fixed

- `readlink` refuses a buffer too small for the target instead of truncating
  the path silently.

## [0.1.0] — 2026-08-25

### Added

- Initial release: a clean-room XFS reader — superblock, allocation groups,
  inodes, extents, and all four directory formats.
- **A blocking real-kernel validation gate in CI.** It found three parser bugs
  before the first release, which is the point of having it.
- Inode parsing is cross-validated against the reference XFS debugger.
- The C ABI, with its tests written alongside it.

[0.12.1]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.12.0...v0.12.1
[0.12.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.11.0...v0.12.0
[0.11.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.10.0...v0.11.0
[0.10.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.9.0...v0.10.0
[0.9.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.8.0...v0.9.0
[0.8.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.2...v0.6.0
[0.5.2]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.1...v0.5.2
[0.5.1]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/antimatter-studios/rust-fs-xfs/compare/v0.1.0...v0.3.0
[0.1.0]: https://github.com/antimatter-studios/rust-fs-xfs/releases/tag/v0.1.0
