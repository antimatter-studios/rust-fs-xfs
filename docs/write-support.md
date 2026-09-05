# What this driver will write, and what it will not

Every row below was decided by running the operation and asking the
kernel and `xfs_repair`, not by reading the format documentation. The
matrix that does it is `tests/feature_matrix_oracle.rs`; the fixtures are
built by `scripts/build-feature-matrix-fixtures.sh`.

**190 combination/operation pairs: 176 written and sound, 9 refused by
name, 5 not applicable, 0 broken.**

## The rule

Two outcomes are acceptable. The write happens and the checker finds
nothing wrong, or the driver refuses **by name** before touching
anything. A filesystem that mounts, behaves, and disagrees with the
checker is not acceptable — that is the failure this whole arrangement
exists to prevent, and it is the one that keeps happening:

| what was wrong | how it presented |
|---|---|
| no reverse-mapping record for an allocation | `Missing reverse-mapping record for (0/13)` |
| blocks freed under another file | `data fork in ino 134 claims free block 24` |
| `di_aformat` left at zero on a new inode | `bad attribute format 0 in inode 138` |
| `di_nblocks` written as 1 for a multi-block directory | `bad nblocks 1 for inode 262208` |
| entry offsets starting at zero | `would have corrected entry offsets in directory 786496` |
| the ordinary name hash on a case-insensitive filesystem | the kernel shut the filesystem down |

None of them failed at the time. Every one mounted and behaved.

## Maintained

`finobt`, `inobtcount`, `rmapbt`, `reflink`, `bigtime`, `nrext64`,
sparse inodes, block sizes 1–4 KiB, inode sizes 512 B and 1 KiB,
directory blocks larger than a filesystem block, case-insensitive
directories, and allocation groups above the first.

`rmapbt` and `reflink` are the two that were refused until recently.
`mkfs.xfs` turns `rmapbt` on by default, so refusing it meant refusing
an ordinary volume.

## Refused, and why

**Writing a v4 filesystem.** Reading one is supported and stays
supported. Writing is refused because the format is finished, and
`mkfs.xfs` says so itself:

```text
V4 filesystems are deprecated and will not be supported by future versions.
```

Building a second set of transaction shapes for a format upstream has
declared dead — and which newer kernels are dropping support for — is
not work this driver should do. If that changes, the matrix already has
the row.

**Shapes no measurement covers**, each refused by name rather than
guessed:

- merging an allocation into an adjacent reverse-mapping record (the
  kernel does this; no operation here can produce the case)
- freeing part of a shared extent, which would split a refcount record
- a copy-on-write staging extent over blocks being freed
- a B+tree more than one level deep, where a record change reshapes a
  node
- an inode chunk that needs allocating, and extents spanning allocation
  groups

## Not applicable

Five pairs: freeing a shared extent on a filesystem formatted
`reflink=0`, where no extent can be shared. Nothing to exercise, rather
than something declined.
