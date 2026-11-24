//! The three API layers every rust-fs driver exposes, for XFS.
//!
//! [`XfsVolume`] implements rust-fs-core's [`VolumeAPI`], [`OpenVolume`],
//! [`PrimitiveAPI`] and [`FileAPI`], and [`XfsSalvage`] its
//! [`SalvageAdapter`]. They are built on this crate's own reads: the strict
//! [`Filesystem`] for the files of a healthy volume, and [`crate::salvage`]
//! for everything that has to work on a damaged one. The XFS-specific
//! primitives (superblock copies, group headers, the group B+trees, inodes,
//! block maps, directories) stay in [`crate::salvage`]; only the
//! format-neutral surface is here. `docs/three-layers.md` is the map.
//!
//! Nothing here writes. Every source is a [`BlockRead`], and a dirty log is
//! replayed in memory by the mount the File layer reads through, never onto
//! the source.

use crate::inode::{FileType, Inode};
use crate::salvage::{self, InodeCandidate, InodeState};
use crate::superblock::{Superblock, XFS_SB_MAGIC, XFS_SB_SIZE};
use crate::{Error as XfsError, Filesystem};
use fs_core::layers::{
    check_bounds, ApiError, Candidate, CandidateKind, CandidateState, Capabilities, DirEntry,
    Extent, ExtentWalk, FileAPI, Geometry, Metadata, NodeKind, OpenVolume, Parent, ParentClaim,
    PrimitiveAPI, Probe, Recognition, SalvageAdapter, SalvageError, ScanReport, Sink, Structure,
    StructureRole, SuperblockCopy, Timestamp, Tree, TreeNode, VolumeAPI, VolumeHealth,
    VolumeSupport,
};
use fs_core::layers::{DamageReport, Feature, Integrity, WalkExtent};
use fs_core::BlockRead;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::Arc;

/// The name [`VolumeAPI::format`] gives.
pub const FORMAT: &str = "xfs";

/// What the layers fail with: a refusal the shared contract names, or this
/// driver's own error.
#[derive(Debug)]
pub enum LayerError {
    Api(ApiError),
    Xfs(XfsError),
}

impl fmt::Display for LayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api(e) => e.fmt(f),
            Self::Xfs(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for LayerError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Api(e) => Some(e),
            Self::Xfs(e) => Some(e),
        }
    }
}

impl From<ApiError> for LayerError {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<XfsError> for LayerError {
    fn from(e: XfsError) -> Self {
        Self::Xfs(e)
    }
}

impl From<fs_core::Error> for LayerError {
    fn from(e: fs_core::Error) -> Self {
        Self::Xfs(e.into())
    }
}

/// An XFS volume opened through the shared layers.
pub struct XfsVolume {
    source: Arc<dyn BlockRead>,
    /// Every superblock copy and the geometry chosen from them.
    found: salvage::Superblocks,
    /// The strict mount, when the volume has one: what the File layer reads.
    fs: Option<Filesystem>,
    /// Why the strict mount was refused, when it was.
    refused: Option<String>,
}

impl XfsVolume {
    /// The strict mount the File layer reads through, when there is one.
    pub fn filesystem(&self) -> Option<&Filesystem> {
        self.fs.as_ref()
    }

    /// The geometry chosen from the superblock copies.
    pub fn geometry_superblock(&self) -> Option<&Superblock> {
        self.found.geometry.as_ref()
    }

    fn mounted(&self) -> Result<&Filesystem, LayerError> {
        self.fs
            .as_ref()
            .ok_or(LayerError::Api(ApiError::Unsupported(
                "files: the volume does not mount; use the salvage verbs",
            )))
    }
}

impl fmt::Debug for XfsVolume {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("XfsVolume")
            .field("geometry", &self.found.source)
            .field("mounted", &self.fs.is_some())
            .field("refused", &self.refused)
            .finish()
    }
}

impl VolumeAPI for XfsVolume {
    type Error = LayerError;
    type Superblock = Superblock;

    fn format(&self) -> &'static str {
        FORMAT
    }

    fn size_bytes(&self) -> u64 {
        self.source.size_bytes()
    }

    fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<(), LayerError> {
        check_bounds(offset, output.len(), self.size_bytes())?;
        self.source.read_at(offset, output)?;
        Ok(())
    }

    fn geometry(&self) -> Result<Geometry, LayerError> {
        let sb = self.superblock()?;
        let label = sb.fname.trim_end_matches('\0').to_string();
        Ok(Geometry {
            sector_size: u32::from(sb.sectsize),
            allocation_unit: u64::from(sb.blocksize),
            allocation_units: sb.dblocks,
            declared_bytes: sb.dblocks.saturating_mul(u64::from(sb.blocksize)),
            layout: None,
            label: (!label.is_empty()).then_some(label),
            serial: Some(uuid_text(&sb.uuid)),
        })
    }

    fn superblock(&self) -> Result<Superblock, LayerError> {
        self.found
            .geometry
            .clone()
            .ok_or(LayerError::Xfs(XfsError::NotXfs { magic: 0 }))
    }

    fn superblocks(&self) -> Result<Vec<SuperblockCopy<Superblock>>, LayerError> {
        Ok(self
            .found
            .copies
            .iter()
            .map(|copy| SuperblockCopy {
                copy: copy.ag.unwrap_or_default(),
                location: copy.location,
                superblock: copy.superblock.clone(),
                integrity: copy.integrity,
            })
            .collect())
    }

    fn support(&self) -> Result<VolumeSupport, LayerError> {
        let mut unsupported = Vec::new();
        if let Some(sb) = &self.found.geometry {
            if sb.rblocks != 0 {
                unsupported.push("data on the realtime device, which is not this source".into());
            }
            if !sb.has_internal_log() {
                unsupported.push("an external log, which is not this source".into());
            }
        }
        if let Some(why) = &self.refused {
            unsupported.push(format!("files: {why}"));
        }
        Ok(VolumeSupport {
            files: self.fs.is_some(),
            salvage: XfsSalvage::capabilities_of(),
            unsupported,
        })
    }

    fn health(&self) -> Result<VolumeHealth, LayerError> {
        let report = salvage::damage(self.source.as_ref());
        Ok(VolumeHealth::from_findings(report.all().cloned().collect()))
    }
}

impl OpenVolume for XfsVolume {
    type Source = Arc<dyn BlockRead>;

    fn probe(source: &dyn BlockRead) -> Result<Probe, fs_core::Error> {
        let mut sector = [0u8; 512];
        source.read_at(0, &mut sector)?;
        let magic = u32::from_be_bytes([sector[0], sector[1], sector[2], sector[3]]);
        if magic != XFS_SB_MAGIC {
            return Ok(Probe {
                format: FORMAT,
                recognition: Recognition::No,
                declared_bytes: None,
                detail: format!("magic {magic:#010x}"),
            });
        }
        let found = salvage::read_superblock(source, Some(0), 0);
        let declared_bytes = found
            .superblock
            .as_ref()
            .map(|sb| sb.dblocks.saturating_mul(u64::from(sb.blocksize)));
        let clean = found.damage.is_empty() && sector.len() >= XFS_SB_SIZE;
        Ok(Probe {
            format: FORMAT,
            recognition: if clean {
                Recognition::Yes
            } else {
                Recognition::Damaged
            },
            declared_bytes,
            detail: found
                .damage
                .first()
                .map_or_else(|| "primary superblock".to_string(), |d| d.detail.clone()),
        })
    }

    /// Mounts the volume strictly: a superblock or log this driver cannot
    /// read refuses it. A dirty log is replayed in memory.
    fn open(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let fs = Filesystem::mount(source.clone())?;
        Ok(Self {
            found: salvage::superblocks(source.as_ref()),
            source,
            fs: Some(fs),
            refused: None,
        })
    }

    /// Opens whatever survives. The salvage verbs work from the superblock
    /// copies alone; the File layer works only when the strict mount does.
    fn open_for_salvage(source: Arc<dyn BlockRead>) -> Result<Self, LayerError> {
        let found = salvage::superblocks(source.as_ref());
        let (fs, refused) = match Filesystem::mount(source.clone()) {
            Ok(fs) => (Some(fs), None),
            Err(e) => (None, Some(e.to_string())),
        };
        Ok(Self {
            source,
            found,
            fs,
            refused,
        })
    }
}

impl PrimitiveAPI for XfsVolume {
    type Adapter = XfsSalvage;
    /// None: every group heads a superblock copy, and a scan finds them when
    /// the primary is gone, so no geometry has to be guessed.
    type Hypothesis = ();

    fn source(&self) -> &dyn BlockRead {
        self.source.as_ref()
    }

    fn adapter(&self) -> Result<XfsSalvage, SalvageError> {
        self.found
            .geometry
            .clone()
            .map(XfsSalvage::new)
            .ok_or(SalvageError::NoGeometry)
    }

    fn structures(&self) -> Result<Vec<Structure>, SalvageError> {
        let mut out: Vec<Structure> = self
            .found
            .copies
            .iter()
            .map(|copy| Structure {
                name: match copy.ag {
                    Some(0) => "primary superblock".to_string(),
                    Some(ag) => format!("superblock copy in group {ag}"),
                    None => format!("superblock at byte {}", copy.location.offset),
                },
                role: StructureRole::Superblock,
                location: copy.location,
                copy: copy.ag.unwrap_or_default(),
                integrity: copy.integrity,
            })
            .collect();
        let Some(sb) = &self.found.geometry else {
            return Ok(out);
        };
        for ag in 0..sb.agcount {
            let headers = salvage::ag_headers(self.source.as_ref(), sb, ag);
            for (name, role, location, integrity) in [
                (
                    "AGF",
                    StructureRole::AllocationMap,
                    headers.agf.location,
                    headers.agf.integrity,
                ),
                (
                    "AGI",
                    StructureRole::RecordTable,
                    headers.agi.location,
                    headers.agi.integrity,
                ),
                (
                    "AGFL",
                    StructureRole::AllocationMap,
                    headers.agfl.location,
                    headers.agfl.integrity,
                ),
            ] {
                out.push(Structure {
                    name: format!("{name} {ag}"),
                    role,
                    location,
                    copy: 0,
                    integrity,
                });
            }
        }
        if sb.has_internal_log() {
            out.push(Structure {
                name: "log".to_string(),
                role: StructureRole::Journal,
                location: Extent::new(
                    sb.fsblock_offset(sb.logstart),
                    u64::from(sb.logblocks) * u64::from(sb.blocksize),
                ),
                copy: 0,
                integrity: Integrity::Unchecked,
            });
        }
        Ok(out)
    }
}

/// What an XFS candidate carries beyond the shared fields: enough to walk it
/// again and to tell it from a reused slot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InodeDetail {
    /// The inode number the record's position gives it.
    pub ino: u64,
    /// `di_mode`, type and permission bits.
    pub mode: u16,
    /// `di_nlink`.
    pub links: u32,
    /// `di_gen`, bumped each time the slot is reused.
    pub generation: u32,
}

/// The salvage verbs for XFS: format context only, never a device.
#[derive(Clone, Debug)]
pub struct XfsSalvage {
    sb: Superblock,
}

impl XfsSalvage {
    /// An adapter for a volume of geometry `sb`, such as one
    /// [`salvage::superblocks`] chose.
    pub fn new(sb: Superblock) -> Self {
        Self { sb }
    }

    /// The geometry the adapter reads with.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    fn capabilities_of() -> Capabilities {
        Capabilities {
            // XFS frees a file's blocks before it frees its inode, so a
            // deleted file's record maps nothing; it has no cluster chains
            // for a plan to name and stores no data compressed; and its
            // superblock copies leave no geometry to guess.
            missing: vec![
                Feature::DeletedExtents,
                Feature::ClusterSteps,
                Feature::EncodedExtents,
                Feature::GeometryHypotheses,
            ],
        }
    }

    /// Re-read the candidate's record from `source`.
    fn reread(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<InodeDetail>,
    ) -> Result<InodeCandidate, Extent> {
        let location = candidate.location;
        let mut buf = vec![0u8; usize::from(self.sb.inodesize)];
        source
            .read_at(location.offset, &mut buf)
            .map_err(|_| location)?;
        Ok(salvage::inodes_candidate(
            &self.sb,
            candidate.detail.ino,
            location,
            &buf,
        ))
    }
}

/// The shared candidate for an XFS inode record.
fn shared(found: InodeCandidate) -> Candidate<InodeDetail> {
    let (mode, links, generation, size) = match &found.inode {
        Some(inode) => (inode.mode, inode.nlink, inode.gen, Some(inode.size)),
        None => (0, 0, 0, None),
    };
    let kind = match found.inode.as_ref().and_then(Inode::file_type) {
        Some(FileType::Regular) => CandidateKind::File,
        Some(FileType::Directory) => CandidateKind::Directory,
        _ => CandidateKind::Other,
    };
    Candidate {
        location: found.location,
        kind,
        state: match found.state {
            InodeState::Live => CandidateState::Live,
            InodeState::Free => CandidateState::Deleted,
        },
        integrity: if found.damage.is_empty() {
            found.integrity
        } else {
            Integrity::Failed
        },
        name: None,
        size,
        detail: InodeDetail {
            ino: found.ino,
            mode,
            links,
            generation,
        },
    }
}

impl SalvageAdapter for XfsSalvage {
    type Detail = InodeDetail;

    fn capabilities(&self) -> Capabilities {
        Self::capabilities_of()
    }

    fn find_candidates(
        &self,
        source: &dyn BlockRead,
        range: Extent,
        sink: &mut dyn Sink<Candidate<InodeDetail>>,
    ) -> Result<ScanReport, SalvageError> {
        match range.end() {
            Some(end) if end <= source.size_bytes() => {}
            _ => return Err(SalvageError::Bounds),
        }
        let scan = salvage::find_inodes(source, &self.sb, range, &mut |found| {
            sink.accept(shared(found)) == ControlFlow::Continue(())
        });
        Ok(ScanReport {
            candidates: scan.candidates,
            unreadable: scan.unreadable,
            stopped_at: scan.stopped_at,
        })
    }

    fn walk_extents(
        &self,
        source: &dyn BlockRead,
        candidate: &Candidate<InodeDetail>,
    ) -> Result<ExtentWalk, SalvageError> {
        if candidate.state == CandidateState::Deleted {
            return Err(SalvageError::Unsupported(Feature::DeletedExtents));
        }
        let found = match self.reread(source, candidate) {
            Ok(found) => found,
            Err(at) => {
                return Ok(ExtentWalk {
                    extents: Vec::new(),
                    claimed: candidate.size.unwrap_or(0),
                    end: salvage::WalkEnd::Unreadable(at),
                })
            }
        };
        let map = salvage::walk_extents(source, &self.sb, &found);
        let pieces: Vec<WalkExtent> = match map.inline {
            Some(inline) => vec![WalkExtent::stored(0, inline)],
            None => map
                .extents
                .iter()
                .map(|piece| WalkExtent {
                    offset: piece.file_offset,
                    source: piece.location,
                    uninitialized: piece.unwritten,
                    encoded: None,
                })
                .collect(),
        };
        // Pieces that continue each other in the file and on the source,
        // and agree on being written, are one extent.
        let mut extents: Vec<WalkExtent> = Vec::new();
        for piece in pieces.into_iter().filter(|p| p.source.length > 0) {
            match extents.last_mut() {
                Some(last)
                    if last.uninitialized == piece.uninitialized
                        && last.offset + last.source.length == piece.offset
                        && last.source.end() == Some(piece.source.offset) =>
                {
                    last.source.length += piece.source.length;
                }
                _ => extents.push(piece),
            }
        }
        Ok(ExtentWalk {
            extents,
            claimed: map.inline.map_or(map.claimed, |inline| inline.length),
            end: map.end,
        })
    }

    fn rebuild_tree(
        &self,
        source: &dyn BlockRead,
        candidates: &[Candidate<InodeDetail>],
    ) -> Result<Tree, SalvageError> {
        let found: Vec<InodeCandidate> = candidates
            .iter()
            .filter_map(|c| self.reread(source, c).ok())
            .collect();
        let named = salvage::rebuild_tree(source, &self.sb, &found);
        // A directory's inode number to its candidate's identity. A claim
        // made by a directory that is not among the candidates has nowhere
        // to point in the shared tree and is left out.
        let by_ino: BTreeMap<u64, u64> = candidates
            .iter()
            .filter(|c| c.state == CandidateState::Live)
            .map(|c| (c.detail.ino, c.location.offset))
            .collect();
        let claims: BTreeMap<u64, Vec<ParentClaim>> = named
            .nodes
            .iter()
            .map(|node| {
                let parents = node
                    .parents
                    .iter()
                    .filter_map(|parent| match parent {
                        salvage::Parent::Root => Some(ParentClaim::live(Parent::Root)),
                        salvage::Parent::Directory { ino, name } => {
                            by_ino.get(ino).map(|&at| ParentClaim {
                                parent: Parent::Candidate(at),
                                name: Some(name.clone()),
                                state: CandidateState::Live,
                            })
                        }
                    })
                    .collect();
                (node.ino, parents)
            })
            .collect();
        Ok(Tree {
            nodes: candidates
                .iter()
                .map(|c| TreeNode {
                    candidate: c.location.offset,
                    parents: if c.state == CandidateState::Live {
                        claims.get(&c.detail.ino).cloned().unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                })
                .collect(),
        })
    }

    fn report_damage(
        &self,
        source: &dyn BlockRead,
        sink: &mut dyn Sink<salvage::Damage>,
    ) -> Result<DamageReport, SalvageError> {
        let report = salvage::damage(source);
        let mut out = DamageReport::default();
        for finding in report.all() {
            out.reported += 1;
            if sink.accept(finding.clone()).is_break() {
                out.stopped = true;
                break;
            }
        }
        Ok(out)
    }

    fn read_extent(
        &self,
        source: &dyn BlockRead,
        extent: Extent,
        offset: u64,
        output: &mut [u8],
    ) -> Result<usize, SalvageError> {
        let end = extent.end().ok_or(SalvageError::Bounds)?;
        if offset > extent.length || end > source.size_bytes() {
            return Err(SalvageError::Bounds);
        }
        let count = ((extent.length - offset) as usize).min(output.len());
        source
            .read_at(extent.offset + offset, &mut output[..count])
            .map_err(SalvageError::Device)?;
        Ok(count)
    }
}

impl FileAPI for XfsVolume {
    /// An inode number.
    type Node = u64;

    fn root(&self) -> Result<u64, LayerError> {
        Ok(self.mounted()?.superblock().rootino)
    }

    fn read_dir(&self, directory: &u64) -> Result<Vec<DirEntry<u64>>, LayerError> {
        let fs = self.mounted()?;
        let (inode, raw) = fs.read_inode_raw(*directory)?;
        if !inode.is_dir() {
            return Err(ApiError::NotADirectory(format!("inode {directory}")).into());
        }
        let mut out = Vec::new();
        for entry in fs.read_dir(&inode, &raw)? {
            if entry.name == b"." || entry.name == b".." {
                continue;
            }
            let child = fs.read_inode(entry.ino)?;
            let (name, lossy) = match String::from_utf8(entry.name.clone()) {
                Ok(name) => (name, false),
                Err(_) => (String::from_utf8_lossy(&entry.name).into_owned(), true),
            };
            out.push(DirEntry {
                name,
                lossy,
                bytes: entry.name,
                kind: node_kind(&child),
                size: child.size,
                node: entry.ino,
            });
        }
        Ok(out)
    }

    fn metadata(&self, node: &u64) -> Result<Metadata, LayerError> {
        let fs = self.mounted()?;
        let inode = fs.read_inode(*node)?;
        let time = |t: crate::inode::Timestamp| Timestamp {
            seconds: t.sec,
            nanoseconds: t.nsec,
        };
        Ok(Metadata {
            kind: node_kind(&inode),
            size: inode.size,
            allocated: Some(
                inode
                    .nblocks
                    .saturating_mul(u64::from(fs.superblock().blocksize)),
            ),
            created: (inode.version == 3).then(|| time(inode.crtime)),
            modified: Some(time(inode.mtime)),
            accessed: Some(time(inode.atime)),
            mode: Some(u32::from(inode.mode & 0o7777)),
            links: Some(u64::from(inode.nlink)),
            attributes: Some(u64::from(inode.flags)),
        })
    }

    fn read_at(&self, file: &u64, offset: u64, output: &mut [u8]) -> Result<usize, LayerError> {
        let fs = self.mounted()?;
        let (inode, raw) = fs.read_inode_raw(*file)?;
        if inode.is_dir() {
            return Err(ApiError::IsADirectory(format!("inode {file}")).into());
        }
        let want = inode.size.saturating_sub(offset).min(output.len() as u64) as usize;
        let mut done = 0;
        while done < want {
            let read = fs.read_at(&inode, &raw, offset + done as u64, &mut output[done..want])?;
            if read == 0 {
                return Err(ApiError::Bounds {
                    offset: offset + done as u64,
                    length: (want - done) as u64,
                    size: inode.size,
                }
                .into());
            }
            done += read;
        }
        Ok(done)
    }

    fn read_link(&self, link: &u64) -> Result<Vec<u8>, LayerError> {
        let fs = self.mounted()?;
        let (inode, raw) = fs.read_inode_raw(*link)?;
        Ok(fs.read_link(&inode, &raw)?)
    }

    fn names_match(&self, stored: &str, wanted: &str) -> bool {
        match &self.fs {
            Some(fs) if fs.superblock().has_case_insensitive_dirs() => {
                stored.eq_ignore_ascii_case(wanted)
            }
            _ => stored == wanted,
        }
    }
}

fn node_kind(inode: &Inode) -> NodeKind {
    match inode.file_type() {
        Some(FileType::Regular) => NodeKind::File,
        Some(FileType::Directory) => NodeKind::Directory,
        Some(FileType::Symlink) => NodeKind::Symlink,
        _ => NodeKind::Special,
    }
}

fn uuid_text(uuid: &[u8; 16]) -> String {
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}
