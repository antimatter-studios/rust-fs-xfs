## Checking

| Salvage reads of a damaged volume (`fs_xfs::salvage`): the primary superblock and every copy, decoded past a failed checksum, needs-repair or an unfinished format, with the geometry chosen by agreement and found by scanning when the primary is gone; each group's AGF, AGI and AGFL checked for magic, checksum, identity and ranges and still decoded; the log's state; every failure a finding with its kind, severity and byte range; nothing written | Experimental | Unreleased (#615) | | `salvage_headers.rs`, `salvage_headers_oracle.rs` |
