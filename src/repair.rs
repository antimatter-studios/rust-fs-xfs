//! Planning a repair, apart from making one (#375).
//!
//! A plan is every byte range a repair would change, with the bytes there
//! now and the bytes it would put there. Making one reads the volume
//! through an ordinary read-only mount and the checker's report, and
//! writes nothing: a dry run is a plan that is printed and not applied.
//! Applying a plan is not this module's business, and nothing here can
//! do it.
//!
//! # What is refused before anything is proposed
//!
//! A plan made from a volume this cannot reason about would be a guess
//! with an exact-looking list of bytes, so each of these refuses the
//! whole plan with a structured finding, and no rule is asked:
//!
//! - **Something else holds the target** ([`Code::RepairNotExclusive`]):
//!   [`Exclusive::claim`] takes an exclusive lock on the file and, on
//!   Linux, refuses a device or image that is mounted or attached to a
//!   loop device. A plan of a volume that is changing under it is stale
//!   before it is printed.
//! - **A feature whose metadata the checker does not validate**
//!   ([`Code::RepairFeature`]): a repair has to keep every structure that
//!   describes what it changes consistent, so a volume carrying one the
//!   checker never reads is refused. That is anything outside
//!   [`INCOMPAT`] and [`RO_COMPAT`], any `log_incompat` bit, a v4 volume,
//!   a realtime section and quota accounting. The finding names the
//!   field.
//! - **A log that needed replay** ([`Code::RepairLogDirty`]): the mount
//!   reads the volume as replay leaves it, in memory, and a repair
//!   written under that view would land on a disk that says something
//!   else.
//! - **A check that did not cover the volume** ([`Code::RepairIncomplete`]):
//!   a partial scan, or findings suppressed from the report, leave damage
//!   no plan accounts for.
//! - **Ambiguous ownership** ([`Code::RepairAmbiguous`]): a block claimed
//!   by two owners, or a directory reached from two places, has no single
//!   owner to repair towards.
//!
//! # Rules
//!
//! What gets proposed comes from [`Rule`]s, each owning the finding codes
//! it repairs. A rule reads the volume and the report and puts the bytes
//! it wants into a [`Proposal`], which records what is there now beside
//! them. Findings no rule owns are listed as [`Plan::unplanned`]: the
//! plan says what it would leave, instead of letting a partial repair
//! read as a complete one.
//!
//! The plan is deterministic: changes are in device order, and refusals
//! and unplanned findings are sorted, so the same volume gives the same
//! plan every time.

use crate::check::{self, Code, Finding, Location, Report, Scan, Severity};
use crate::superblock::{incompat, ro_compat};
use crate::{Error, Filesystem, Result};
use fs_core::BlockRead;
use std::collections::BTreeMap;
use std::path::Path;

/// Every `sb_features_incompat` bit a plan may be made under: the ones
/// whose metadata the checker reads, or that change no structure it
/// walks.
pub const INCOMPAT: u32 = incompat::FTYPE
    | incompat::SPINODES
    | incompat::META_UUID
    | incompat::BIGTIME
    | incompat::NREXT64;

/// Every `sb_features_ro_compat` bit a plan may be made under.
///
/// Not `RMAPBT`: every block a repair moves has a reverse-mapping record
/// that would have to move with it, and the checker does not read the
/// reverse-mapping btree.
pub const RO_COMPAT: u32 = ro_compat::FINOBT | ro_compat::REFLINK | ro_compat::INOBTCNT;

/// Proof that the caller has the target to itself.
///
/// A plan takes one so that "nothing else is changing this volume" is a
/// thing the caller did, not a thing the planner hoped.
#[derive(Debug)]
pub struct Exclusive {
    _held: Option<std::fs::File>,
}

impl Exclusive {
    /// Take `path` for this process: an exclusive lock on the file, held
    /// until the returned value is dropped, and, on Linux, no mount or
    /// loop device using it.
    ///
    /// # Errors
    ///
    /// A [`Code::RepairNotExclusive`] finding saying who has it.
    pub fn claim(path: &Path) -> std::result::Result<Self, Finding> {
        let refused = |what: String| Finding {
            code: Code::RepairNotExclusive,
            location: Location::default(),
            what,
        };
        let file = std::fs::File::open(path)
            .map_err(|e| refused(format!("{} could not be opened: {e}", path.display())))?;
        match file.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(refused(format!(
                    "another process holds a lock on {}",
                    path.display()
                )))
            }
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(refused(format!(
                    "{} could not be locked: {e}",
                    path.display()
                )))
            }
        }
        if let Some(user) = in_use(path) {
            return Err(refused(format!("{} is {user}", path.display())));
        }
        Ok(Exclusive { _held: Some(file) })
    }

    /// The caller's word that nothing else uses the device: for a device
    /// this process made, such as one in memory, that has no path to lock.
    pub fn asserted_by_caller() -> Self {
        Exclusive { _held: None }
    }
}

/// Whether a plan was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Every precondition held, and every rule finished.
    Ready,
    /// A precondition failed or a rule could not finish: there are no
    /// changes, and [`Plan::refusals`] says why.
    Refused,
}

impl Status {
    /// The status as it appears in the output.
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ready => "ready",
            Status::Refused => "refused",
        }
    }
}

/// One byte range a repair would change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    /// Byte offset on the device.
    pub offset: u64,
    /// What the device holds there now.
    pub before: Vec<u8>,
    /// What the repair would put there. The same length as `before`.
    pub after: Vec<u8>,
    /// The finding this change repairs.
    pub code: Code,
    /// The rule that proposed it.
    pub rule: &'static str,
    /// Why, in words.
    pub what: String,
}

/// What a repair would do, and what it would leave.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Whether a plan was made.
    pub status: Status,
    /// Every proposed change, in device order. Empty unless `Ready`.
    pub changes: Vec<Change>,
    /// Why no plan was made, sorted. Empty when `Ready`.
    pub refusals: Vec<Finding>,
    /// Error findings no rule repairs, sorted.
    pub unplanned: Vec<Finding>,
    /// The check the plan was made from.
    pub check: Report,
}

/// A repair rule: the findings it owns, and the bytes it would write.
pub trait Rule {
    /// The rule's name, as each of its changes records it.
    fn name(&self) -> &'static str;
    /// The finding codes this rule repairs.
    fn repairs(&self) -> &'static [Code];
    /// Put every change this rule would make into `proposal`.
    ///
    /// # Errors
    ///
    /// Anything that stops the rule from finishing its plan. The whole
    /// plan is then refused: a rule that planned half a repair has not
    /// planned one.
    fn propose(&self, fs: &Filesystem, report: &Report, proposal: &mut Proposal) -> Result<()>;
}

/// The changes the rules have proposed so far.
pub struct Proposal<'a> {
    device: &'a dyn BlockRead,
    rule: &'static str,
    changes: BTreeMap<u64, Change>,
}

impl Proposal<'_> {
    /// Propose `after` at byte `offset`, recording what is there now.
    ///
    /// A change to what is already there is dropped. The same rule
    /// proposing the same range again replaces its earlier proposal.
    ///
    /// # Errors
    ///
    /// The device's read error, or [`Error::UnsupportedFeature`] when the
    /// range overlaps a different change, or one another rule proposed:
    /// two proposals for the same bytes are two answers, and a plan
    /// holding both would hold neither.
    pub fn put(
        &mut self,
        offset: u64,
        after: Vec<u8>,
        code: Code,
        what: impl Into<String>,
    ) -> Result<()> {
        let len = after.len() as u64;
        let end = offset.checked_add(len).ok_or_else(|| {
            Error::UnsupportedFeature(format!(
                "a change of {len} bytes at {offset} ends past the address space"
            ))
        })?;
        let mut before = vec![0u8; after.len()];
        self.device.read_at(offset, &mut before)?;
        let clash = self
            .changes
            .range(..end)
            .next_back()
            .filter(|(&at, c)| at + c.after.len() as u64 > offset)
            .map(|(_, c)| c);
        if let Some(c) = clash {
            if c.offset != offset || c.after.len() != after.len() || c.rule != self.rule {
                return Err(Error::UnsupportedFeature(format!(
                    "{} proposes {len} bytes at {offset}, which {} already proposes to change \
                     at {} ({} bytes)",
                    self.rule,
                    c.rule,
                    c.offset,
                    c.after.len()
                )));
            }
        }
        if before == after {
            self.changes.remove(&offset);
            return Ok(());
        }
        self.changes.insert(
            offset,
            Change {
                offset,
                before,
                after,
                code,
                rule: self.rule,
                what: what.into(),
            },
        );
        Ok(())
    }
}

/// Plan a repair of `fs` with no rules: the preconditions, and every
/// error finding listed as unplanned.
pub fn plan(fs: &Filesystem, access: &Exclusive) -> Plan {
    plan_with(fs, access, &[])
}

/// Plan a repair of `fs` with `rules`, in the order given. Never writes.
pub fn plan_with(fs: &Filesystem, _access: &Exclusive, rules: &[&dyn Rule]) -> Plan {
    let report = check::check(fs);
    let mut refusals = preconditions(fs, &report);
    let mut changes = BTreeMap::new();
    if refusals.is_empty() {
        let mut proposal = Proposal {
            device: fs.device(),
            rule: "",
            changes: BTreeMap::new(),
        };
        for rule in rules {
            proposal.rule = rule.name();
            if let Err(e) = rule.propose(fs, &report, &mut proposal) {
                refusals.push(Finding {
                    code: Code::RepairIncomplete,
                    location: Location::default(),
                    what: format!("rule {} could not finish its plan: {e}", rule.name()),
                });
                break;
            }
        }
        if refusals.is_empty() {
            changes = proposal.changes;
        }
    }
    let owned: Vec<Code> = rules
        .iter()
        .flat_map(|r| r.repairs().iter().copied())
        .collect();
    let mut unplanned: Vec<Finding> = report
        .findings
        .iter()
        .filter(|f| f.severity() == Severity::Error && !owned.contains(&f.code))
        .cloned()
        .collect();
    sort(&mut refusals);
    sort(&mut unplanned);
    Plan {
        status: if refusals.is_empty() {
            Status::Ready
        } else {
            Status::Refused
        },
        changes: changes.into_values().collect(),
        refusals,
        unplanned,
        check: report,
    }
}

/// Everything that stops a plan being made, before any rule is asked.
fn preconditions(fs: &Filesystem, report: &Report) -> Vec<Finding> {
    let sb = fs.superblock();
    let mut refusals = Vec::new();
    let mut feature = |field: &'static str, what: String| {
        refusals.push(Finding {
            code: Code::RepairFeature,
            location: Location {
                field: Some(field),
                ..Location::default()
            },
            what,
        })
    };
    if !sb.is_v5() {
        feature(
            "sb_versionnum",
            "a v4 volume's metadata carries no checksum or owner, so a repair cannot \
             tell a block it should change from one that only looks like it"
                .into(),
        );
    } else {
        let extra = sb.features_incompat & !INCOMPAT;
        if extra != 0 {
            feature(
                "sb_features_incompat",
                format!("incompatible features {extra:#x} are outside what a plan reasons about"),
            );
        }
        let extra = sb.features_ro_compat & !RO_COMPAT;
        if extra != 0 {
            feature(
                "sb_features_ro_compat",
                format!(
                    "read-only-compatible features {extra:#x} are outside what a plan \
                     reasons about"
                ),
            );
        }
        if sb.features_log_incompat != 0 {
            feature(
                "sb_features_log_incompat",
                format!(
                    "log-incompatible features {:#x} are set, so the log holds items a \
                     plan does not reason about",
                    sb.features_log_incompat
                ),
            );
        }
    }
    if sb.rblocks != 0 {
        feature(
            "sb_rblocks",
            format!(
                "a realtime section of {} blocks is not checked, so a repair cannot keep it \
                 consistent",
                sb.rblocks
            ),
        );
    }
    if sb.qflags != 0 {
        feature(
            "sb_qflags",
            format!(
                "quota flags {:#x} are set, and a repair that changes what an inode owns \
                 would leave its quota usage wrong",
                sb.qflags
            ),
        );
    }
    if fs.was_replayed() {
        refusals.push(Finding {
            code: Code::RepairLogDirty,
            location: Location::default(),
            what: "the log held records that had not been applied; mount the volume so the \
                   kernel replays them, then plan again"
                .into(),
        });
    }
    if report.scan == Scan::Partial {
        refusals.push(Finding {
            code: Code::RepairIncomplete,
            location: Location::default(),
            what: "the check could not read everything, so what it did not read could \
                   hold damage no plan accounts for"
                .into(),
        });
    } else if report.suppressed > 0 {
        refusals.push(Finding {
            code: Code::RepairIncomplete,
            location: Location::default(),
            what: format!(
                "{} findings were suppressed from the report, so a plan cannot account \
                 for them",
                report.suppressed
            ),
        });
    }
    for f in &report.findings {
        if matches!(f.code, Code::CrossLink | Code::DirReachedTwice) {
            refusals.push(Finding {
                code: Code::RepairAmbiguous,
                location: f.location.clone(),
                what: format!("{}: {}", f.code.as_str(), f.what),
            });
        }
    }
    refusals
}

/// Sort findings into one order that does not depend on how they were
/// found.
fn sort(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        let key = |f: &Finding| {
            (
                f.code,
                f.location.ag,
                f.location.agbno,
                f.location.ino,
                f.location.field,
            )
        };
        key(a).cmp(&key(b)).then_with(|| a.what.cmp(&b.what))
    });
}

/// Who else is using `path`, where that can be told: on Linux, a mount
/// whose source it is, or a loop device it backs.
#[cfg(target_os = "linux")]
fn in_use(path: &Path) -> Option<String> {
    let target = std::fs::canonicalize(path).ok()?;
    if let Ok(info) = std::fs::read_to_string("/proc/self/mountinfo") {
        if let Some(at) = mounted_in(&info, &target) {
            return Some(format!("mounted at {at}"));
        }
    }
    for entry in std::fs::read_dir("/sys/block").ok()?.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("loop") {
            continue;
        }
        if let Ok(backing) = std::fs::read_to_string(entry.path().join("loop/backing_file")) {
            if Path::new(backing.trim_end_matches('\n')) == target {
                return Some(format!("attached to /dev/{}", name.to_string_lossy()));
            }
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn in_use(_path: &Path) -> Option<String> {
    None
}

/// The mount point of the first mount in `mountinfo` whose source is
/// `target`.
///
/// Each line is `ID PARENT MAJ:MIN ROOT MOUNTPOINT OPTIONS [TAGS...] -
/// FSTYPE SOURCE SUPEROPTIONS`, with spaces and other awkward bytes in a
/// path written as three-digit octal escapes (`\040` for a space).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn mounted_in(mountinfo: &str, target: &Path) -> Option<String> {
    for line in mountinfo.lines() {
        let Some((before, after)) = line.split_once(" - ") else {
            continue;
        };
        let source = after.split(' ').nth(1).map(unescape);
        if source.as_deref().map(Path::new) == Some(target) {
            return Some(
                before
                    .split(' ')
                    .nth(4)
                    .map(unescape)
                    .unwrap_or_else(|| "an unnamed mount point".into()),
            );
        }
    }
    None
}

/// Undo mountinfo's `\NNN` octal escapes.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 4 <= bytes.len() {
            let digits = &bytes[i + 1..i + 4];
            if digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                let value = digits.iter().fold(0u32, |v, d| v * 8 + u32::from(d - b'0'));
                if let Ok(b) = u8::try_from(value) {
                    out.push(b);
                    i += 4;
                    continue;
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTINFO: &str = "\
22 1 8:2 / / rw,relatime shared:1 - ext4 /dev/sda2 rw\n\
40 22 7:0 / /mnt/with\\040space rw,relatime shared:9 - xfs /dev/loop0 rw,attr2\n\
41 22 0:5 / /dev rw,nosuid - devtmpfs devtmpfs rw\n\
42 22 8:17 / /data rw - xfs /srv/img/a\\040b.img rw\n";

    #[test]
    fn a_mounted_source_is_found_with_its_mount_point() {
        assert_eq!(
            mounted_in(MOUNTINFO, Path::new("/dev/loop0")).as_deref(),
            Some("/mnt/with space")
        );
        assert_eq!(
            mounted_in(MOUNTINFO, Path::new("/dev/sda2")).as_deref(),
            Some("/")
        );
    }

    #[test]
    fn an_escaped_source_matches_the_path_it_names() {
        assert_eq!(
            mounted_in(MOUNTINFO, Path::new("/srv/img/a b.img")).as_deref(),
            Some("/data")
        );
    }

    #[test]
    fn an_unmounted_path_and_a_prefix_are_not_mounts() {
        assert_eq!(mounted_in(MOUNTINFO, Path::new("/dev/sdb1")), None);
        assert_eq!(mounted_in(MOUNTINFO, Path::new("/dev/sda")), None);
    }

    #[test]
    fn unescape_leaves_a_short_or_bad_escape_alone() {
        assert_eq!(unescape("a\\040b"), "a b");
        assert_eq!(unescape("a\\04"), "a\\04");
        assert_eq!(unescape("a\\9xy"), "a\\9xy");
        assert_eq!(unescape("tail\\"), "tail\\");
    }
}
