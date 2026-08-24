# Working in rust-fs-xfs (agent guide)

Pure-Rust XFS driver (`fs-xfs`) exposing a C ABI (`fs_xfs_*`). This file is the fast
path for an agent adding or fixing functionality, so the workflow doesn't have to be
re-derived each time. It points at the existing docs rather than duplicating them:

- **README** → `## Byte order`, `## Status`, `## Test contract`, `## Building`.

## The one rule that matters here

**Never validate this driver against fixtures the driver built itself.**

Unit tests that parse in-process fixtures prove self-consistency, not correctness. A
misreading of the on-disk format gets baked into the fixture *and* the parser, and they
agree with each other while disagreeing with every real filesystem. Three bugs shipped
past a fully green unit suite for exactly this reason:

| Bug | Why the unit tests missed it |
|---|---|
| Superblock magic had two bytes transposed (`0x58425346` for `0x58465342`) | Fixtures were written with the same wrong constant |
| Checksums read big-endian; XFS stores *checksums* little-endian (`~cpu_to_le32`) | Fixtures wrote them big-endian too |
| Checksum computed over the 264-byte struct, not the whole sector | Fixtures were 264 bytes |

All three died on the first comparison against `xfs_db`. Any new structure you parse
gets a cross-validation test in `tests/oracle_vm_fixtures.rs` before it is considered
done.

## Byte order

XFS is **big-endian on disk** — the only big-endian format in this crate family. Use
`from_be_bytes`, never `from_le_bytes`, never a raw struct cast. The sole exception is
checksum fields, which are little-endian; `superblock::le32` exists for those and
should not be used for anything else.

## Skills to use

- **`dev-loop`** — required for any non-trivial change: baseline the full suite →
  change → re-run (no baseline test may regress) → enhance tests → vet.
- **`commit`** / **`pr`** — for grouping commits and opening PRs. Commit subject plus
  flat one-sentence bullets; **no AI attribution or co-author trailers**.
- Discipline for **bug fixes**: **prove it's broken first** (a failing check), *then*
  fix, *then* prove the same check is green, *then* confirm the full baseline passes.
  Never write the fix before you have a red.

## Running tests

```sh
cargo test                                   # unit + host-side oracle tests
cargo test --test oracle_vm_fixtures -- --nocapture   # just the cross-validation
cargo test -- --ignored                      # tests that shell out to xfsprogs (Linux)
cargo clippy --all-targets -- -D warnings    # what the pre-commit hook runs
```

Install hooks once per clone: `./scripts/install-hooks.sh`.

## The oracle VM

`mkfs.xfs` / `xfs_db` are Linux-only, so on macOS they live in a Debian arm64 VM
(QEMU + HVF, hardware-accelerated).

```sh
./scripts/vm.sh up               # boot; idempotent
./scripts/vm-build-fixtures.sh   # regenerate .vm-share/ fixture matrix
./scripts/vm.sh run <cmd>        # run a command in the guest
./scripts/vm.sh down             # halt (next `up` is fast)
```

Fixtures land in `.vm-share/` as `xfs-<name>.img` + `xfs-<name>.sbdump`. They are
gitignored; the tests skip cleanly when absent.

### Two VM traps, both already paid for

1. **Never set `config.notify_forwarder.enable = false`.** The plugin's `up` hook
   truncates the QEMU boot chain — the VM imports successfully and then never boots,
   printing no error at all. Either leave it enabled or don't load the plugin.
2. **`qe.virtiofs_guest_uid`/`gid` must match the box's `vagrant` user** (1001 on this
   box, not the plugin's 1000 default), or the shared folder is read-only to the guest
   and every fixture build fails with a bare permission error.

Also: `generic/debian12` is deliberately not used — it was rebuilt upstream without its
UEFI bootloader and no longer boots on a fresh clone.

## Adding a parsed structure

1. Derive field offsets from the format documentation. Do not trust recalled constants —
   two of the three bugs above were exactly that.
2. Where a `log2` companion field exists (`blocklog` beside `blocksize`, etc), assert
   they agree. That redundancy is the cheapest detector for a wrong offset or wrong
   byte order, and it is why XFS carries it.
3. On v5, verify both the CRC **and** the self-describing identity fields (UUID and
   owning AG). The checksum catches corrupted bits; the identity fields catch an
   intact block that came from the wrong place.
4. Add a cross-validation case to `tests/oracle_vm_fixtures.rs`, and a geometry to
   `scripts/vm-build-fixtures.sh` if the structure only appears under a particular
   mkfs option.

## Project rules

- **No GPL/LGPL/AGPL dependencies.** Permissive only (MIT/BSD/Apache). Shelling out to
  a copyleft CLI as a *test oracle* is fine — linking or copying is not.
- **This is a standalone project.** Never mention any consuming application in the
  README, source, or CLI help.
- Apple Silicon is the only target architecture for consumers of this crate; the crate
  itself is portable and CI runs on x86_64 Linux, which is fine because on-disk formats
  are endian-defined rather than host-defined.
