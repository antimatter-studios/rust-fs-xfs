# Releases, and verifying one

A release is a version tag. `.github/workflows/release.yml` runs the branch gate on it,
publishes the crate to crates.io, attaches the `.crate` to the GitHub release for the
tag, and builds the command-line tools for it. The library and the tools share one
version, one tag and one [CHANGELOG.md](../CHANGELOG.md) section, so no binary can wrap
a library other than the one it was tagged with.

## What a release ships

- **The crate**, `rust-fs-xfs` on crates.io, and the same `.crate` file on the
  GitHub release page.
- **The command-line tools**, as `rust-fs-xfs-<version>-<platform>.tar.gz` for
  `darwin-arm64` and `linux-x86_64`. Each is an install prefix: `bin/rust-fs-xfs` and
  the `fs.xfs`, `mkfs.xfs` and `fsck.xfs` links to it; man pages (section 1 for
  `fs.xfs` and its verbs, section 8 for `mkfs.xfs` and `fsck.xfs`) and zsh, bash and fish
  completions under `share/`; `share/rust-fs-xfs/CAVEATS`; and `LICENSE`. The pages and
  completions are written by the binary itself (`rust-fs-xfs generate man|completions
  SHARE`) from the argument definitions it parses with, so they cannot describe a flag
  it does not take. What the tarball holds is declared in `Cargo.toml` under
  `[package.metadata.package-cli]`; rust-fs-core's release-cli workflow packages,
  attests and attaches it.

## Verifying a download

Every version published to crates.io is also attached to the GitHub release for its
tag, with a build-provenance attestation signed by this repository's release workflow.
It proves the crate was built by `.github/workflows/release.yml` from a commit in this
repository, not uploaded from someone's machine. To check the crates.io download of
version `X.Y.Z`:

```sh
curl -sSfLo rust-fs-xfs-X.Y.Z.crate https://static.crates.io/crates/rust-fs-xfs/rust-fs-xfs-X.Y.Z.crate
gh attestation verify rust-fs-xfs-X.Y.Z.crate \
  --repo antimatter-studios/rust-fs-xfs \
  --signer-workflow antimatter-studios/rust-fs-xfs/.github/workflows/release.yml
```

The workflow refuses to attest a `.crate` whose sha256 differs from the checksum
crates.io records for that version, so the file on the release page and the crates.io
download are the same bytes. `tests/release_attestation.rs` fails a pull request that
removes the attestation or widens the grants it needs.

The command-line tarballs on the same release page are attested by rust-fs-core's
release-cli workflow, which this repository's release workflow calls, so that is the
signer to name:

```sh
gh attestation verify rust-fs-xfs-X.Y.Z-darwin-arm64.tar.gz \
  --repo antimatter-studios/rust-fs-xfs \
  --signer-workflow antimatter-studios/rust-fs-core/.github/workflows/release-cli.yml
```

Both commands were checked against the 0.14.1 release: the tarball verifies with
rust-fs-core's release-cli workflow as the signer and not with this repository's
release workflow, and the `.crate` the other way round.
