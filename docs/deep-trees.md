# Writing to a group whose trees are more than one block

Every write path in this driver refuses when one of an allocation
group's four B+trees is more than one level deep:

> allocation group 0's by-block free-space tree is 2 levels deep, where
> taking a record out can collapse a node; only a single-level tree is
> supported

There are 27 such refusals across `group_write`, `create`, `unlink`,
`truncate` and `file_write`. They are honest, and they are also the most
limiting thing about the driver: a 4 KiB root holds 505 free-space
records or 252 inode records, so *any* filesystem with real
fragmentation has a level-2 tree and gets a refusal where it should get
a write. Reading was fixed separately (`ag_btree::walk`); this is about
changing one.

## What the shape of a fix has to deal with

Editing a record in a multi-level tree is three problems, not one:

1. **Finding the leaf.** A record no longer lives in the root.
2. **Keeping the tree legal after the edit.** Adding a record to a full
   leaf splits it; removing one from a minimally-filled leaf merges it.
   Either changes the parent, which may split or merge in turn, which
   may add or remove a level.
3. **Finding blocks for the tree itself.** A split needs a new block,
   and it cannot come from the free-space tree, because the free-space
   tree is what is being edited. XFS's answer is the **AGFL**: a small
   ring of blocks held aside in the group for exactly this, refilled
   outside the edit.

## The approach: lay the tree out again

Read every record in the tree, edit the list in memory, and lay a fresh
tree out over it. Blocks that come out the same are logged as no change,
because a buffer item is a diff against the before-image.

The alternative is the kernel's: a cursor that descends, splits and
merges in place, propagating keys upward. It moves fewer bytes and it is
a great deal more code, with the interesting cases — a split that
propagates to a new root, a merge that removes a level — reachable only
through fixtures built to reach them.

Laying it out again has none of those cases. There is one function that
turns N records into a tree, and it produces the same tree whatever the
tree looked like before, so the edge cases are *arithmetic* rather than
control flow: how many leaves, how full, how many levels.

What it costs:

- **Memory.** The whole record list at once. A group holds at most
  `agblocks / 2` free-space records; at the 400 MB fixtures that is
  megabytes, and on a 1 TB filesystem with 32 groups the worst case is
  tens of megabytes for a pathologically fragmented group.
- **Log traffic.** A record inserted near the front shifts every record
  after it, so most leaves differ and most leaves are logged. The
  kernel would log three blocks. This is the real price, and it is
  bounded by the size of the tree.

Both are bounded, and neither is a correctness risk. A transaction whose
rebuild would exceed the log reservation must be refused rather than
attempted, so there is a ceiling on the number of blocks one rebuild may
log — a refusal that names the tree and the count, in place of a
refusal that names the depth.

## What the tree has to satisfy

`xfs_repair` is the judge, and it checks more than "the records are
there":

- every non-root block holds at least `maxrecs / 2` records;
- the level a block states is one below its parent's;
- each block's `bb_blkno` is its own address, its owner is the group,
  its UUID is the filesystem's, and its CRC is right;
- `agf_roots`, `agf_levels` and `agf_btreeblks` describe the tree that
  is actually there;
- the blocks the tree occupies are accounted for — they are not free
  space, and with `rmapbt` they carry an `XFS_RMAP_OWN_AG` record.

## The AGFL

Measured on `xfsdeep-bno2.img`, a 400 MB filesystem at 1 KiB blocks with
600 one-block files created and every second one deleted:

```
bnolevel = 2      cntlevel = 2      btreeblks = 6
flfirst = 7       fllast = 14       flcount = 8
freeblks = 204144 longest = 203832
```

Allocating one block through the kernel afterwards moved `freeblks` to
204143 and left the levels, `btreeblks` and the whole free list
untouched: an edit that only shrinks a record needs no new tree block.
So the AGFL matters for growth and shrinkage of the tree, not for every
edit.

Blocks the rebuild needs come off the free list; blocks it no longer
needs go back on. The list is refilled from free space at the end, which
is a second edit of the same trees — done once the rebuild has settled,
against the record list, not the disk.

## Order of work

1. **Fixtures** — `build-deeptree-fixtures.sh`, which fragments a group
   until its trees are two levels and *checks with `xfs_db` that they
   are*, discarding the image if not. A fixture built to prove a shape
   and holding a different one is a test that passes for the wrong
   reason. **Done.**
2. **Layout** — records to blocks, and its inverse, with the fill rules
   above. Pure arithmetic, unit-tested.
3. **AGFL** — take and put, with the ring arithmetic and the header.
4. **One editor** — reading a group's trees, editing records, laying
   them out again, and emitting the items. `GroupAlloc` already does
   this for the single-level case and grows into it.
5. **The 27 sites** — each one stops checking the depth.
6. **Judgement** — the deep-tree fixtures through the feature matrix,
   the write oracles and `xfs_repair`.
