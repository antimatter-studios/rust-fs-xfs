# rust-fs-xfs

A pure-Rust driver for the [XFS](https://docs.kernel.org/filesystems/xfs/index.html)
filesystem. It reads v5 and v4 volumes, writes v5 volumes through the log in records
the Linux kernel replays, checks and repairs them, and formats new ones. It works on
an image file or a device directly, with no kernel driver and no mount.

It is a Rust library (`fs_xfs`), a static library with a stable C ABI (`fs_xfs_*`,
declared in [`include/fs_xfs.h`](include/fs_xfs.h)) for C, C++, Swift or Go, and a set
of command-line tools: `fs.xfs`, `mkfs.xfs` and `fsck.xfs`. Block devices come from the
shared [`rust-fs-core`](https://github.com/antimatter-studios/rust-fs-core) trait, so
the same device, partition slice or disk-image reader serves every driver in that
family.

It is validated against the reference tools (`xfs_db`, `xfs_repair`, `xfs_logprint`)
and the Linux kernel, not only against its own tests.

## What it can do

**[docs/features.md](docs/features.md) is the full list**: every feature, its state
(supported, experimental, partial, refused, not supported or upcoming), the release it
shipped in, its tracking issue and the test that checks it. Every pull request that
changes behaviour updates it. In summary:

### Reading

- v5 and v4 volumes, clean or with a dirty log, which is replayed in memory with the
  device untouched.
- Directories in every form (short form, block, leaf and node), looked up through
  their hash index; extent and B+tree data forks; inline and remote symlinks.
- Extended attributes in every form, remote values included, and POSIX ACLs in the
  format the Linux VFS presents; parent pointers; realtime files, given the realtime
  device; reverse-mapping and reference-count btrees.
- Every v5 metadata header is checked for its CRC32C and for its identity (UUID,
  allocation group, block number). An incompatible feature the driver does not know is
  refused by name at mount rather than misread.
- Names and paths are bytes, never assumed to be UTF-8.

### Checking and repair

- `fs_xfs::check`, and `fsck.xfs` over it, check a volume without writing: the
  secondary superblocks; each allocation group's headers against its free-space,
  inode, reverse-mapping and refcount btrees; one owner for every block (a block
  claimed twice is a cross-link unless reflink shares it, a block claimed by nothing is
  lost); every allocated inode; the directory tree and link counts; the superblock's
  counters; realtime metadata; and quota records and accounting. It is a subset of what
  `xfs_repair -n` checks, and its finding codes are graded against it.
- Findings carry stable codes, a severity and a location, and the report says whether
  the scan was complete, partial or impossible. A partial scan is never clean.
- `fs_xfs::repair` plans a repair as byte ranges with their bytes before and after, and
  applies it: superblock copies, group and superblock counters, inode allocation and
  link counts, and directory metadata. A volume it cannot reason about (no exclusive
  access, a v4 volume, a feature the checker does not cover such as a realtime section
  or quotas, a dirty log, an incomplete scan, ambiguous ownership) is refused, and
  nothing is written.

### Writing and journalling

- An overwrite of bytes that already exist changes no metadata and is written in
  place, on v5 and v4.
- Everything else is v5 only, and is written as records in the volume's log, which the
  Linux kernel replays: creating files, directories and symlinks; writing anywhere in a
  file (holes filled, unwritten extents converted, the file grown); truncating to any
  length; unlinking, removing directories and hard links; renaming within a directory,
  across directories and over an existing name; times, permissions and ownership;
  extended attributes and POSIX ACLs; exchanging ranges of two files' data; keeping
  parent pointers on every namespace change; and user, group and project quota
  accounting with hard limits.
- The label is written into the superblock of every allocation group, and logged when
  the mount has already logged a change.
- These work in every directory form and in groups whose btrees are more than one
  block deep, and are tested against every legal combination of features, reflink and
  reverse mapping included.
- Many operations can be logged in one mount, the log being reused in bounded memory,
  and a crash at any checkpoint replays to a prefix of them. `Filesystem::unmount`
  writes the metadata home and ends the log with an unmount record, so the kernel
  mounts the volume with no replay.
- A volume whose log still needs replaying is refused for writing.
- Many of the write operations are marked **Experimental** on the features page: they
  pass every test against the kernel and `xfs_repair`, but are new, and some rows name
  a limit.

### Formatting

- `fs_xfs::mkfs`, and `mkfs.xfs` over it, make a v5 filesystem with the standard
  formatter's default features (checksums, the free inode btree, sparse inode chunks,
  reflink, large timestamps, directory entry file types), with 1, 2 or 4 KiB blocks, on
  a device or image of at least 300 MiB. On the same device with the same UUID, every
  allocation group header and btree root is byte-identical to the standard formatter's.

### C ABI

- Mounting by path, over caller-supplied read callbacks, or over an `fs_core` device
  (which is how a partition inside a whole disk is mounted), with or without a realtime
  device; volume information, `stat`, directory iteration, reading files and
  `readlink`.
- Writable mounts and the journalled operations above: create, mkdir, symlink, unlink,
  rmdir, link, rename, writing and truncating files, attributes, extended attributes,
  the label and a flush; `fs_xfs_umount` ends a writable mount cleanly.
- The checker (`fs_xfs_check`) and the repair (`fs_xfs_repair_with_fs_core_device`),
  each passing findings or planned changes to a callback.

### Tools

- `fs.xfs` lists, reads, writes and inspects a volume; `mkfs.xfs` formats one;
  `fsck.xfs` checks one and repairs what it can; `rust-fs-xfs doctor` says whether the
  tools on `PATH` are this program. Each has a man page and shell completions.

Not done: resizing (`fs.xfs resize` answers `not implemented`), journalled writes to a
v4 volume, and writing realtime files.

## Building

```sh
cargo build --release                  # the library: libfs_xfs.a and the rlib
cargo build --release --features cli   # and the command-line tools
cargo clippy --all-targets --features cli -- -D warnings
```

The crate builds as both an `rlib` and a `staticlib`, so it links into a Rust
dependency graph or alongside sibling drivers in a C, Swift or Go program. The
toolchain is pinned in `rust-toolchain.toml`. A checkout builds against the sibling
`../rust-fs-core` checkout, which `chore siblings` clones and moves to the ref pinned
in `chores.yml`; the crate published on crates.io depends on `rust-fs-core` from
crates.io instead. `chore staticlib` builds the library and its headers (`fs_xfs.h`
and `fs_core.h`) into this crate's own `dist/`, and `chore artifact` prints the
absolute path of that directory — a consumer copies its contents rather than being
told where cargo puts things.

The `cli` feature adds the command-line tools and their argument parser and nothing
else, so the library a consumer links gains no dependency from it.

### Git hooks

Install the git hooks once per clone:

```sh
~/.claude/skills/github-guard/install.sh .
```

The guards live in `.git/hooks`, outside the working tree, so no branch
checkout can rewrite the hook that is about to run. They are per-clone
rather than tracked: re-run the installer in a fresh clone, and after the
guards are updated.

## Using it

### From C

```c
#include <stdio.h>
#include "fs_xfs.h"

int main(void) {
    fs_xfs_fs_t *fs = fs_xfs_mount("disk.img");
    if (!fs) {
        fprintf(stderr, "mount: %s\n", fs_xfs_last_error());
        return 1;
    }

    fs_xfs_dir_iter_t *dir = fs_xfs_dir_open(fs, "/etc");
    const fs_xfs_dirent_t *e;
    while (dir && (e = fs_xfs_dir_next(dir)) != NULL)
        printf("%s\n", e->name);
    fs_xfs_dir_close(dir);

    char buf[4096];
    int64_t n = fs_xfs_read_file(fs, "/etc/fstab", buf, 0, sizeof buf);
    if (n < 0)
        fprintf(stderr, "read: %s (errno %d)\n", fs_xfs_last_error(), fs_xfs_last_errno());
    else
        fwrite(buf, 1, (size_t)n, stdout);

    fs_xfs_umount(fs);
    return 0;
}
```

```sh
cargo build --release
cc -Iinclude app.c target/release/libfs_xfs.a -o app
```

That is the whole link line on macOS; on another platform add the system libraries
`cargo rustc --release --lib --crate-type staticlib -- --print native-static-libs`
lists. `chore staticlib` puts the same library and headers in `dist/`. A function returning `int` returns 0 on success and -1 on failure, and one
returning a pointer returns `NULL`; `fs_xfs_last_error()` and `fs_xfs_last_errno()` then
give a message and a POSIX errno for the calling thread. Writing needs a handle from
`fs_xfs_mount_rw` or `fs_xfs_mount_rw_with_fs_core_device`, and `fs_xfs_umount` on a
writable handle writes everything home. [`include/fs_xfs.h`](include/fs_xfs.h)
documents every function and its errors.

### From Rust

```toml
[dependencies]
rust-fs-xfs = "0.14"
rust-fs-core = "0.3"
```

```rust
use std::sync::Arc;

use fs_core::{BlockDevice, BlockRead, FileDevice};
use fs_xfs::Filesystem;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Read-only: list a directory, read a file, check the volume.
    let device: Arc<dyn BlockRead> = Arc::new(FileDevice::open("disk.img")?);
    let fs = Filesystem::mount(device)?;
    for entry in fs.open("/etc")?.entries()? {
        println!("{} (inode {})", String::from_utf8_lossy(&entry.name), entry.ino);
    }
    let fstab: Vec<u8> = fs.open("/etc/fstab")?.read_all()?;
    println!("{} bytes", fstab.len());
    let report = fs_xfs::check::check(&fs);
    println!("clean: {}", report.is_clean());
    drop(fs);

    // Read-write: every change is a log record; unmount writes it home.
    let device: Arc<dyn BlockDevice> = Arc::new(FileDevice::open_rw("disk.img")?);
    let fs = Filesystem::mount_rw(device)?;
    let root = fs.root()?.inode().ino;
    let (ino, _) = fs.create_file(root, b"notes.txt", 0o100_644)?;
    fs.write(ino, 0, b"hello\n")?;
    fs.unmount()?;
    Ok(())
}
```

The library is `fs_xfs` and `rust-fs-core`'s is `fs_core`. `Filesystem::open_bytes`
takes a path as bytes, for names that are not UTF-8. `fs_xfs::repair` plans and
applies a repair, and `fs_xfs::mkfs::format` makes a filesystem.

### From the command line

The tools are one multi-call binary, `rust-fs-xfs`, and `fs.xfs`, `mkfs.xfs` and
`fsck.xfs` are links to it; `rust-fs-xfs fs ...` (and `mkfs`, `fsck`) is the same
program under the one name nothing else on `PATH` can shadow, and `rust-fs-xfs doctor`
says whether every name `PATH` resolves is this program and, if not, what wins and the
fix. Each release attaches a tarball for `darwin-arm64` and `linux-x86_64` holding the
binary, its links, man pages and shell completions ([docs/releases.md](docs/releases.md));
from a checkout, `chore cli:install` stages the same under `tmp/cli/`.

**`fs.xfs`** works inside an XFS image or device directly: no mount, no kernel driver.
It is an escape hatch for an errand (get a file out, read the label, check whether the
log needs replaying), not a place to do real filesystem work: for that, mount it.

```sh
fs.xfs disk.img ls /etc              # a directory, as JSON
fs.xfs disk.img read /etc/fstab > fstab
fs.xfs disk.img write /notes.txt < notes.txt
fs.xfs disk.img get                  # the properties, as JSON
fs.xfs disk.img get label --text     # one of them, for a person
fs.xfs --offset 1048576 whole-disk.img info
```

| verb | what |
|---|---|
| `ls [path]` | a directory (`/` by default): each entry's `name`, `type`, `size`, `mode`, `mtime` and `inode`, and a symlink's `target`; v4 and v5 |
| `read <path> [-o FILE]` | the file's raw bytes on stdout, or into FILE; v4 and v5 |
| `write <path>` | the bytes on stdin. A new path becomes a file in one extent (v5); an empty file is given its contents; a file of exactly the same length is overwritten in place (v4 and v5). Anything else -- appending, shortening, a v4 create, a file too large for one free run -- is refused with the reason (exit 3), and the volume is left as it was |
| `mkdir <path>` | a directory, mode 0755, whose parent must exist (v5) |
| `get [key]`, `info [key]` | `fs`, `label`, `total_bytes`, `free_bytes`, `block_size`, `dirty` (the log holds records nothing has applied yet), and `xfs` or `xfs.<field>` |
| `set label <value>` | writes the label (at most 12 bytes) into the superblock of every allocation group; a volume whose log is not clean is refused |
| `resize <size>` | answers `not implemented` (exit 3): the library has no resize |

`--offset BYTES` says where the filesystem starts, for a partition inside a whole-disk
image. Metadata is JSON on stdout by default, `--text` for people. A failure is
`{"error": "...", "code": N}` on stderr, `N` being the exit status: 1 failed, 2 the
command line was wrong, 3 not implemented or refused.

A journalled write -- a new file, a filled empty file, a directory -- lands as a record
in the log: the kernel applies it the next time Linux mounts the volume, and `get dirty`
says `true` until then. The driver writes only to a volume whose log is clean, so the
next journalled write on the same image is refused until it has been mounted once.
Reading needs no such wait: `ls` and `read` replay the log in memory.

**`mkfs.xfs`** makes a v5 filesystem with the standard formatter's default features on
a device or image of at least 300 MiB. The target must already exist at its size,
unless `--size` is given for an image file that does not exist yet; a JSON report of
what was written goes to stdout, and progress to stderr.

```sh
mkfs.xfs --size 1G -L BACKUP disk.img
truncate -s 4G disk.img && mkfs.xfs -b size=4096 -d agcount=8 disk.img
mkfs.xfs -f /dev/sdb1         # replace whatever is there
mkfs.xfs -N disk.img          # the geometry, written nowhere
```

It takes `-b size=` (1, 2 or 4 KiB), `-d agcount=`, `-L`, `-m uuid=`, `-f`, `-q`, `-N`
and `--size`. Any other option the standard formatter accepts is refused by name rather
than ignored, so the filesystem made is the one asked for. A device that already holds
a filesystem or a partition table is refused without `-f`.

**`fsck.xfs`** checks a volume and reports what is wrong with it, writing nothing
unless a repair is asked for.

```sh
fsck.xfs disk.img              # the report, as JSON
fsck.xfs --text disk.img       # the findings, one per line
fsck.xfs --dry-run disk.img    # what a repair would change, writing nothing
fsck.xfs -y disk.img           # repair what a rule owns
fsck.xfs --offset 1048576 whole.img   # a partition inside a disk image
```

`-n` checks only, which is the default. `--dry-run` takes the target for itself, plans
a repair and prints it under the report's `plan` key. `-y` and `-p` make the same plan
and apply it; a refused plan writes nothing. The exit status is fsck(8)'s: 0 clean,
1 errors corrected, 4 errors left (5 when some were corrected and some left), 8 the
target could not be checked, 16 a wrong command line, which includes `-y` or `-p` asked
for together with `-n` or `--dry-run`. [docs/fsck-output.md](docs/fsck-output.md) is
the report's schema, the finding codes and the repair rules.

## How it is tested

Unit tests prove the parsers agree with themselves; they cannot prove they agree with
XFS, because a misreading of the format is baked into the fixture and the parser alike.
So every structure the driver parses or writes is also checked against something that
is not this crate: images made by the reference `mkfs.xfs` and read back field by field
against `xfs_db`; what the driver writes graded by `xfs_repair -n` and mounted by the
Linux kernel, which replays every log record the driver wrote. Those tools run in one
Linux guest from
[fs-linux-test-harness](https://github.com/antimatter-studios/fs-linux-test-harness),
the same on a developer's machine and in CI. A test that cannot reach the tool, fixture
or VM it needs fails rather than skipping, and the parsers are fuzzed.

```sh
chore siblings     # ../rust-fs-core and ../fs-linux-test-harness at their pinned refs
chore test:unit    # no tool, no fixture, no VM
chore test         # everything, exactly as CI runs it
```

[docs/testing.md](docs/testing.md) has the tiers, the fixtures and how they are made;
[docs/on-disk-format.md](docs/on-disk-format.md) has the format details that are easy
to get wrong, and which bugs they caused here.

## Documentation

- [docs/features.md](docs/features.md): every feature and its state.
- [docs/fsck-output.md](docs/fsck-output.md): `fsck.xfs`'s report, codes and repair.
- [docs/on-disk-format.md](docs/on-disk-format.md): byte order, checksums and
  self-describing metadata.
- [docs/transaction-shapes.md](docs/transaction-shapes.md),
  [docs/buffer-log-item.md](docs/buffer-log-item.md) and
  [docs/logged-inode.md](docs/logged-inode.md): what each operation logs, as measured.
- [docs/testing.md](docs/testing.md): the test contract, tiers and fixtures.
- [docs/releases.md](docs/releases.md): what a release ships and how to verify it.
- [AGENTS.md](AGENTS.md): how to work on this repository.

## Changelog

The latest releases; every release, with the reasoning behind each change, is in [CHANGELOG.md](CHANGELOG.md).

### v0.14.2 — 2026-10-17

- Volumes with parent pointers are written: every namespace change keeps them, past short form and beside values stored in blocks of their own, and `Filesystem::parent_pointers` reads them.
- An exchange moves reflinked blocks and edits block-map trees.
- The repair is reachable through the C ABI: `fs_xfs_repair_with_fs_core_device` plans a repair on an `fs_core` device and, asked to, writes it.
- The checker leaves a group it cannot read unchecked rather than reporting its inodes lost, and `fsck.xfs` names a target it cannot open in every mode.

### v0.14.1 — 2026-10-14

- Two files exchange ranges of their data by exchanging blocks (`exchange_range`), on volumes with the exchange-range feature, which are now written.

### v0.14.0 — 2026-10-13

- `fsck.xfs -y` repairs superblock copies, counters, inode allocation and directory metadata, on volumes with a reverse-mapping btree too.
- The checker is reachable through the C ABI: `fs_xfs_check` reports each finding and whether the scan was complete.
- A symlink can be created through the log, and a label set after a logged change is logged.
- `fs_xfs_attr_t` reports each time's nanoseconds, and a full volume is ENOSPC rather than ENOTSUP.

### v0.13.0 — 2026-10-12

- The C ABI writes through the log: a writable device mount, create, mkdir, unlink, rmdir, link, rename, a clean unmount, and attribute and extended-attribute changes.
- A write anywhere in a file, truncate to any length, directories past one block and node-form directories changed in place.
- Hard links, renames across directories and over an existing name, and empty directories removed.
- POSIX ACLs and extended attributes written and removed on v5.
- The checker reads realtime metadata, the reverse-mapping and refcount btrees and quota records, every code graded by `xfs_repair -n`.

### v0.12.1 — 2026-10-07

- The tools are released again.
- The family's scripts run in place from rust-fs-core, and this repository keeps no copy.
- A release's notes are its CHANGELOG section.
- rust-fs-core 0.3.7.

### v0.12.0 — 2026-10-06

- Published as `rust-fs-xfs`, the repository's name.
- Depends on `rust-fs-core` 0.3.0.

### v0.11.0 — 2026-10-06

- The last version published as `am-fs-xfs`.
- `fsck.xfs`, and `fs_xfs::check` under it (#339).
- `fs.xfs set label` and `Filesystem::set_label` (#341).
- `mkfs.xfs`, and `fs_xfs::mkfs` under it (#338).
- `Error::InvalidGeometry`.

### v0.10.0 — 2026-10-01

- `Error` gains `RealtimeDeviceAbsent { ino }`.
- A create in a directory with a default ACL is refused.
- A POSIX ACL is listed as root lists it through the kernel.

### v0.9.0 — 2026-09-30

- In-image paths cross the C ABI as bytes, not UTF-8 (#269).
- `fs.xfs`, a command-line tool over the library.

### v0.8.0 — 2026-09-27

- `fs_xfs_readlink` follows the readlink contract every driver in the family now shares.
- The parsers are fuzzed, on two tiers.
- A checkpoint larger than one in-core log buffer is written as several records.
- A volume with a dirty log mounts, replaying into memory.
- A directory takes entries after it has outgrown the inode.

## License

MIT — see [LICENSE](LICENSE).
