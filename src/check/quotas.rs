//! Quota inode cross-references and accounting over the allocated inode walk.

use super::Checker;
use super::Code;
use crate::endian::be16;
use crate::inode::{Format, Inode};
use crate::quota::{Kind, Record, RECORD_SIZE};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Default, Debug, PartialEq, Eq)]
struct Usage {
    blocks: u64,
    inodes: u64,
    realtime: u64,
}

impl Checker<'_> {
    pub(super) fn quotas(&mut self, inodes: &BTreeMap<u64, Inode>, raws: &HashMap<u64, Vec<u8>>) {
        let sb = self.fs.superblock().clone();
        if sb.qflags & !0x7ff != 0
            || (sb.is_v5() && sb.qflags & 0x30 != 0)
            || (!sb.is_v5() && sb.qflags & 0x780 != 0)
        {
            self.find(
                Code::QuotaFlags,
                None,
                None,
                format!(
                    "quota flags {:#x} are invalid for this superblock version",
                    sb.qflags
                ),
            );
        }
        if !sb.is_v5() && sb.qflags & 0x48 == 0x48 {
            self.find(
                Code::QuotaFlags,
                None,
                None,
                "v4 cannot account group and project quotas together".into(),
            );
        }
        let mut seen = HashSet::new();
        let quota_inodes: HashSet<_> = [sb.uquotino, sb.gquotino, sb.pquotino]
            .into_iter()
            .filter(|&ino| ino != 0 && ino != u64::MAX)
            .collect();
        // Comparing a partial inode walk would manufacture counter mismatches.
        let complete = inodes.len() == self.allocated.len();
        for mut kind in [Kind::User, Kind::Group, Kind::Project] {
            let ino = kind.inode(&sb);
            let active = sb.qflags & kind.accounting() != 0;
            if ino == 0 || ino == u64::MAX {
                if active {
                    self.quota_find(
                        Code::QuotaInode,
                        kind,
                        ino,
                        "accounting is enabled but the quota inode is missing".into(),
                    );
                }
                continue;
            }
            if !seen.insert(ino) {
                self.quota_find(
                    Code::QuotaInode,
                    kind,
                    ino,
                    "is also referenced by another quota type".into(),
                );
                continue;
            }
            let Some(inode) = inodes.get(&ino) else {
                self.quota_find(
                    Code::QuotaInode,
                    kind,
                    ino,
                    "is not a readable allocated inode".into(),
                );
                continue;
            };
            if !inode.is_regular_file()
                || !matches!(inode.format, Format::Extents | Format::Btree)
                || inode.is_realtime()
                || inode.nlink != 1
                || ino == sb.rootino
                || ino == sb.rbmino
                || ino == sb.rsumino
            {
                self.quota_find(
                    Code::QuotaInode,
                    kind,
                    ino,
                    "has an invalid type, fork format, link count or metadata cross-reference"
                        .into(),
                );
                continue;
            }
            let extents = match self.fs.data_extents(inode, &raws[&ino]) {
                Ok(e) => e,
                Err(e) => {
                    self.quota_find(
                        Code::QuotaUnreadable,
                        kind,
                        ino,
                        format!("cannot read its extent map: {e}"),
                    );
                    continue;
                }
            };
            // A disabled v4 project quota retains its inode in gquotino;
            // the legacy flags no longer distinguish the two namespaces.
            if !sb.is_v5() && kind == Kind::Group && sb.qflags & 0x48 == 0 {
                if let Some(e) = extents.first() {
                    if let Ok(block) = self.fs.read_fsblock(e.startblock) {
                        if block[3] & 7 == 2 {
                            kind = Kind::Project;
                        }
                    }
                }
            }
            let compare = active && complete && sb.qflags & kind.checked(sb.is_v5()) != 0;
            let mut usage = BTreeMap::<u32, Usage>::new();
            let mut usage_complete = true;
            if compare {
                for (&number, inode) in inodes {
                    if quota_inodes.contains(&number) {
                        continue;
                    }
                    let id = match kind {
                        Kind::User => inode.uid,
                        Kind::Group => inode.gid,
                        Kind::Project => project_id(
                            &raws[&number],
                            sb.features2 & crate::superblock::features2_flags::PROJID32BIT != 0,
                        ),
                    };
                    let rt = if inode.is_realtime() {
                        let (start, end) = inode.data_fork_range(usize::from(sb.inodesize));
                        let fork = &raws[&number][start..end];
                        let mapped = match inode.format {
                            Format::Extents => crate::extent::parse_list(fork, inode.nextents),
                            Format::Btree => {
                                crate::bmbt::walk(fork, inode.nextents, &sb, number, |b| {
                                    self.fs.read_fsblock(b)
                                })
                            }
                            _ => Err(crate::Error::BadSuperblock(
                                "invalid realtime extent fork".into(),
                            )),
                        };
                        match mapped {
                            Ok(e) => e.iter().map(|e| e.blockcount).sum(),
                            Err(e) => {
                                self.quota_find(
                                    Code::QuotaUnreadable,
                                    kind,
                                    ino,
                                    format!("cannot count realtime usage of inode {number}: {e}"),
                                );
                                usage_complete = false;
                                continue;
                            }
                        }
                    } else {
                        0
                    };
                    let Some(blocks) = inode.nblocks.checked_sub(rt) else {
                        self.quota_find(
                            Code::QuotaUsage,
                            kind,
                            ino,
                            format!("inode {number} has more realtime blocks than its block count"),
                        );
                        usage_complete = false;
                        continue;
                    };
                    let counted = usage.entry(id).or_default();
                    match (
                        counted.blocks.checked_add(blocks),
                        counted.realtime.checked_add(rt),
                    ) {
                        (Some(blocks), Some(realtime)) => {
                            counted.blocks = blocks;
                            counted.realtime = realtime;
                            counted.inodes += 1;
                        }
                        _ => {
                            self.quota_find(
                                Code::QuotaUsage,
                                kind,
                                ino,
                                format!("usage of ID {id} overflows a quota counter"),
                            );
                            usage_complete = false;
                        }
                    }
                }
            }
            let per_block = u64::from(sb.blocksize) / RECORD_SIZE as u64;
            let mut records = HashSet::new();
            for e in extents {
                if e.unwritten {
                    self.quota_find(
                        Code::QuotaInode,
                        kind,
                        ino,
                        format!(
                            "has an unwritten quota extent at logical block {}",
                            e.startoff
                        ),
                    );
                    continue;
                }
                for n in 0..e.blockcount {
                    let logical = e.startoff + n;
                    if logical > u64::from(u32::MAX) / per_block {
                        self.quota_find(
                            Code::QuotaInode,
                            kind,
                            ino,
                            format!("logical quota block {logical} is outside the quota ID space"),
                        );
                        break;
                    }
                    let block = match self.fs.read_fsblock(e.startblock + n) {
                        Ok(b) => b,
                        Err(e) => {
                            self.quota_find(
                                Code::QuotaUnreadable,
                                kind,
                                ino,
                                format!("cannot read logical quota block {logical}: {e}"),
                            );
                            continue;
                        }
                    };
                    for slot in 0..per_block {
                        let wide_id = logical * per_block + slot;
                        let id = wide_id as u32;
                        let at = slot as usize * RECORD_SIZE;
                        match Record::parse(
                            &block[at..at + RECORD_SIZE],
                            kind,
                            id,
                            sb.is_v5().then_some(&sb.meta_uuid),
                            sb.features_incompat & crate::superblock::incompat::BIGTIME != 0,
                        ) {
                            Ok(record) => {
                                if wide_id > u64::from(u32::MAX) {
                                    continue;
                                }
                                records.insert(id);
                                if compare && usage_complete {
                                    let zero = Usage::default();
                                    let counted = usage.get(&id).unwrap_or(&zero);
                                    for (name, found, want) in [
                                        ("blocks", record.blocks.count, counted.blocks),
                                        ("inodes", record.inodes.count, counted.inodes),
                                        (
                                            "realtime blocks",
                                            record.realtime.count,
                                            counted.realtime,
                                        ),
                                    ] {
                                        if found != want {
                                            self.quota_find(Code::QuotaUsage, kind, ino, format!("record {id} counts {found} {name}; allocated inodes account for {want}"));
                                        }
                                    }
                                }
                            }
                            Err(e) => {
                                // This slot exists even when its record is malformed;
                                // do not also label it missing below.
                                records.insert(id);
                                self.quota_find(
                                    Code::QuotaInode,
                                    kind,
                                    ino,
                                    format!("record {id}: {e}"),
                                );
                            }
                        }
                    }
                }
            }
            if compare && usage_complete {
                for id in usage.keys() {
                    if !records.contains(id) {
                        self.quota_find(
                            Code::QuotaUsage,
                            kind,
                            ino,
                            format!("no record for ID {id}, which owns allocated inodes"),
                        );
                    }
                }
            }
        }
    }

    fn quota_find(&mut self, code: Code, kind: Kind, ino: u64, detail: String) {
        let ag = if ino != 0 && ino != u64::MAX {
            Some(self.fs.superblock().split_ino(ino).0)
        } else {
            None
        };
        self.find(
            code,
            ag,
            Some(ino),
            format!("{} quota inode {ino}: {detail}", kind.name()),
        );
    }
}

fn project_id(raw: &[u8], wide: bool) -> u32 {
    u32::from(be16(raw, 20))
        | if wide {
            u32::from(be16(raw, 22)) << 16
        } else {
            0
        }
}

#[cfg(test)]
#[path = "quota_tests.rs"]
mod accounting_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_id_uses_both_big_endian_halves_only_with_projid32() {
        let mut raw = [0; 24];
        raw[20..24].copy_from_slice(&[0x12, 0x34, 0x56, 0x78]);
        assert_eq!(project_id(&raw, true), 0x5678_1234);
        assert_eq!(project_id(&raw, false), 0x1234);
    }
}
