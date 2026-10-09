# fsck output contract

`fsck.xfs` emits JSON by default; `--text` emits human-readable lines with
finding codes. The checker never writes. This contract describes schema
`rust-fs-xfs/fsck`, version `1`.

## Versioning

The report always includes `schema` and integer `schema_version`. Removing or
renaming a key, changing its type or meaning, requires a new schema version.
Additive keys and new finding codes do not; consumers must ignore unknown keys
and handle unknown codes. Codes are never renamed or reused for another fault.
Retired codes remain reserved. Human descriptions (`what`) may change freely.

## Report

| Key | Type | Meaning |
|---|---|---|
| `schema` | string | `rust-fs-xfs/fsck`. |
| `schema_version` | integer | `1`. |
| `fs` | string | `xfs`. |
| `device` | string | The command-line target. |
| `clean` | boolean | Complete scan with no error findings or suppressed findings. |
| `dirty` | boolean | The log required in-memory replay; the target was not changed. |
| `scan` | string | `complete`, `partial`, or `none`, as below. |
| `exit` | integer | Process status: `0` clean or `4` findings left uncorrected. |
| `inodes` | integer | Allocated inodes walked; omitted when scan is `none`. |
| `directories` | integer | Directories walked; omitted when scan is `none`. |
| `free_blocks` | integer | Free blocks counted; omitted when scan is `none`. |
| `findings` | array | Findings in traversal order. |
| `suppressed` | integer | Findings omitted after 20 of one code in one allocation group. |
| `plan` | object | Only with `--dry-run`: the repair plan, below. |

`complete` means every structure reached by the documented checker subset was
read. It does not claim all invariants checked by `xfs_repair -n` were checked.
In particular, after log replay the superblock counters are intentionally not
compared; `dirty` and the `log.replayed` warning disclose this.

`partial` means a traversal could not read or decode a required structure;
its contents remain unchecked. `clean` is always false, even if findings have
been suppressed. `none` means mounting failed before any checker walk; the
report contains a `mount` finding and `clean` is false. Counts in a partial
report describe only the part walked and must not be treated as volume totals.
A successful mount or an empty finding list alone never establishes clean.

Failure to open/read the target, or a non-XFS target, produces an operational
error on stderr with exit `8`, without a report. Usage errors and unsupported
repair requests (`-y`, `-p`) exit `16`. A mount refusal for an unsupported XFS
feature is currently a `mount` finding, exit `4`; inspect its human description.
No progress or cancellation API is introduced by this schema. `--dry-run` adds
a `plan` key, below; nothing is ever written.

## Repair plan

`fsck.xfs --dry-run` plans a repair and prints the plan. It never writes, and
it applies nothing: the plan is what a repair would do. It first takes the
target for itself: an exclusive lock on the file and, on Linux, no mount or
loop device using it. If that fails, the report has scan `none`, a `plan`
with status `refused` and a `repair.not-exclusive` refusal, and exit `8`.
Otherwise the exit status is the check's.

| Key | Type | Meaning |
|---|---|---|
| `status` | string | `ready`, or `refused` when a precondition failed. |
| `changes` | array | Proposed changes in device order; empty unless `ready`. |
| `refusals` | array | Findings saying why no plan was made, sorted; empty when `ready`. |
| `unplanned` | array | Error findings no repair rule owns, sorted. A `ready` plan with these leaves them. |

Each change has `offset` and `length` (bytes on the device), `code` (the
finding it repairs), `rule` (the rule that proposed it), `before_crc32c` and
`after_crc32c` (CRC32C of the bytes there now and of the bytes proposed), and `what`.
Refusals and unplanned entries are findings, with the keys below.

The plan is deterministic: the same volume gives the same `plan` byte for byte.
A plan is refused, before any rule is asked, for:

- `repair.not-exclusive`: another holder has the target, or it is mounted.
- `repair.feature`: a v4 volume, an incompatible or read-only-compatible
  feature outside `ftype`, `sparse`, `meta_uuid`, `bigtime`, `nrext64`,
  `finobt`, `reflink` and `inobtcount` (so `rmapbt` is refused), any
  log-incompatible feature, a realtime section, or quota flags. `field` names
  the superblock field.
- `repair.log-dirty`: the log needed replay.
- `repair.incomplete`: the scan was partial, findings were suppressed, or a
  rule could not finish its plan.
- `repair.ambiguous`: a `cross-link` or `dir.reached-twice` finding; its
  location is carried over.

## Findings

Every finding has these keys; unknown location values are JSON `null`, never
zero sentinels. Values are integers without loss of 64-bit inode precision;
consumers should use an integer-capable JSON parser.

| Key | Type | Meaning |
|---|---|---|
| `code` | string | Stable fault identifier from the catalogue below. |
| `severity` | string | `error` or `warning`; warnings alone do not imply damage. |
| `ag` | integer or null | Allocation group, including an inode's owning group. |
| `agbno` | integer or null | Block within that group, when known. |
| `ino` | integer or null | Inode concerned, when known. |
| `field` | string or null | On-disk field explicitly named by the finding, when known. |
| `what` | string | Human-readable description; not an automation identifier. |

For example, a cross-link finding can contain:

```json
{"code":"cross-link","severity":"error","ag":0,"agbno":42,"ino":129,
 "field":null,"what":"block 42 of group 0 belongs to two owners"}
```

Locations are partial: an inode checksum failure identifies the inode and its
group, but may leave `agbno` null. A superblock counter concerns the whole
filesystem and has a null group. No location is inferred from description text.

## Codes

The partial-scan column states whether this finding prevents a complete walk.
`repair.*` codes appear only in a plan's `refusals`, never in `findings`.
`mount` is serialized by the CLI with scan `none` instead of `partial`.

| Code | Severity | Partial scan | Meaning |
|---|---|---|---|
| `checksum` | error | yes | A metadata block or inode failed its CRC. |
| `identity` | error | yes | A metadata block's self-describing header names another place. |
| `sb.copy.unreadable` | error | yes | A secondary superblock could not be read or parsed. |
| `sb.copy.field` | error | no | A secondary superblock disagrees with the primary on a field. |
| `sb.copy.uuid` | error | no | A secondary superblock carries another filesystem's UUID. |
| `ag.agf.unreadable` | error | yes | A group's AGF could not be read. |
| `ag.agi.unreadable` | error | yes | A group's AGI could not be read. |
| `ag.agfl.unreadable` | error | yes | A group's free list could not be read. |
| `ag.length` | error | no | The AGF, the AGI and the geometry disagree on a group's length. |
| `btree.unreadable` | error | yes | A btree could not be walked. |
| `inobt.record` | error | yes | An inode btree record could not be decoded. |
| `inobt.chunk-count` | error | no | An inode chunk's counts disagree with its masks. |
| `finobt.mismatch` | error | no | The free inode btree is not the inode btree's chunks with a free inode. |
| `freesp.empty` | error | no | A free-space record is empty. |
| `freesp.overlap` | error | no | A free-space record overlaps or precedes the one before it. |
| `freesp.cnt-order` | error | no | The free-space-by-count btree is out of order. |
| `freesp.disagree` | error | no | The two free-space btrees hold different extents. |
| `counter.agf.freeblks` | error | no | The AGF's free block count is not what the free-space btree holds. |
| `counter.agf.longest` | error | no | The AGF's longest free extent is not the longest one there is. |
| `counter.agf.btreeblks` | error | no | The AGF's count of free-space btree blocks is wrong. |
| `counter.agi.inodes` | error | no | The AGI's inode or free inode count is not what the inode btree holds. |
| `counter.agi.iblocks` | error | no | The AGI's count of inode btree blocks is wrong. |
| `counter.agi.fblocks` | error | no | The AGI's count of free inode btree blocks is wrong. |
| `counter.sb.icount` | error | no | The superblock's inode count is not what the groups add up to. |
| `counter.sb.ifree` | error | no | The superblock's free inode count is not what the groups add up to. |
| `counter.sb.fdblocks` | error | no | The superblock's free block count is not what the groups add up to. |
| `range.block` | error | no | Blocks are claimed outside the group, or outside any group. |
| `range.extent` | error | no | An inode maps an extent outside one allocation group. |
| `cross-link` | error | no | A block is claimed by two owners. |
| `lost` | error | no | Blocks are claimed by nothing. |
| `extent.unreadable` | error | yes | An inode's extent list or extent tree could not be read. |
| `inode.unreadable` | error | yes | An inode could not be read. |
| `inode.free-in-use` | error | no | An inode the inode btree calls free is in use. |
| `inode.allocated-unused` | error | no | An inode the inode btree calls allocated is not in use. |
| `inode.nlink` | error | no | An inode's link count is not the number of entries that reach it. |
| `dir.root` | error | yes | The root directory is missing or unreadable. |
| `dir.not-a-directory` | error | yes | An inode reached as a directory is not one. |
| `dir.unreadable` | error | yes | A directory's entries could not be read. |
| `dir.entry-target` | error | no | A directory entry points at an inode that is not in use. |
| `dir.entry-ftype` | error | no | A directory entry's recorded type is not its inode's. |
| `dir.reached-twice` | error | no | A directory is reached from more than one place. |
| `dir.unreached` | error | no | An allocated inode is reached by no directory. |
| `log.replayed` | warning | no | The log held records that had not been applied; the volume was checked as replaying them leaves it, and its counters were not. |
| `mount` | error | yes | The filesystem could not be mounted, so nothing was checked. |
| `rt.geometry` | error | no | The superblock's realtime fields disagree with each other. |
| `rt.extent` | error | no | A realtime file maps an extent outside the realtime section. |
| `rt.cross-link` | error | no | Two realtime files map one realtime extent. |
| `rt.bitmap` | error | no | The realtime bitmap calls an extent free that a file maps, or in use when nothing maps it. |
| `rt.summary` | error | no | The realtime summary is not what the bitmap adds up to. |
| `counter.sb.frextents` | error | no | The superblock's free realtime extent count is not what the bitmap holds. |
| `rmap.missing` | error | no | A block has an owner the reverse-mapping btree does not record. |
| `rmap.stale` | error | no | The reverse-mapping btree records an owner for a block nothing owns, or that is free. |
| `rmap.owner` | error | no | The reverse-mapping btree records a block under one owner while the walk found it owned by another. |
| `rmap.duplicate` | error | no | The reverse-mapping btree records the same owner of a block twice. |
| `refcount.stale` | error | no | The refcount btree records blocks as shared that one owner or none holds. |
| `refcount.count` | error | no | The refcount btree's count for shared blocks is not the number of file mappings the walk finds. |
| `repair.not-exclusive` | warning | no | No repair was planned: another holder has the target, or it is mounted. |
| `repair.feature` | warning | no | No repair was planned: the volume uses a feature the planner does not reason about. |
| `repair.log-dirty` | warning | no | No repair was planned: the log needed replay. |
| `repair.incomplete` | warning | no | No repair was planned: the check did not cover the volume, or a rule could not finish. |
| `repair.ambiguous` | warning | no | No repair was planned: a block or directory has two owners. |
| `quota.flags` | error | no | The superblock's quota flags are not valid for its version. |
| `quota.inode` | error | no | A quota inode is missing, shared between quota types, not a quota file, maps blocks a quota file cannot have, or holds a record that fails its checksum, UUID, identity or field checks. |
| `quota.unreadable` | error | yes | A quota inode's extent map or records, or the usage they are checked against, could not be read. |
| `quota.usage` | error | no | Quota accounting is not what the allocated inodes add up to. |
