//! The repair planner proposes and never writes (#375).
//!
//! A plan is made from a read-only mount and the checker's report. It
//! lists every byte range a repair would change, with the bytes there now
//! and the bytes it would put there, and it changes nothing. Before it
//! proposes anything it refuses, with a structured finding, a volume it
//! cannot reason about: a feature whose metadata the checker does not
//! validate, a log that needed replay, a scan that did not finish, or
//! ownership that two structures claim.
//!
//! These cases need no tool and no fixture: the volume is made by this
//! crate's own `mkfs` on a sparse in-memory device. What the reference
//! tools say about real damage is `tests/cli_repair_plan_oracle.rs`.

use fs_core::{BlockDevice, BlockRead};
use fs_xfs::check::{Code, Report};
use fs_xfs::repair::{self, Exclusive, Proposal, Rule, Status};
use fs_xfs::Filesystem;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const BYTES: u64 = 300 * 1024 * 1024;
const SECTOR: u64 = 512;

/// A device that keeps only the sectors written to it, and counts every
/// write and flush made after it is armed.
struct Sparse {
    sectors: Mutex<BTreeMap<u64, Vec<u8>>>,
    armed: std::sync::atomic::AtomicBool,
    writes: AtomicU64,
}

impl Sparse {
    fn new() -> Arc<Self> {
        Arc::new(Sparse {
            sectors: Mutex::new(BTreeMap::new()),
            armed: false.into(),
            writes: AtomicU64::new(0),
        })
    }
}

impl BlockRead for Sparse {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_core::Result<()> {
        let sectors = self.sectors.lock().unwrap();
        for (i, byte) in buf.iter_mut().enumerate() {
            let at = offset + i as u64;
            *byte = sectors
                .get(&(at / SECTOR))
                .map_or(0, |s| s[(at % SECTOR) as usize]);
        }
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        BYTES
    }
}

impl BlockDevice for Sparse {
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_core::Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        let mut sectors = self.sectors.lock().unwrap();
        for (i, &byte) in buf.iter().enumerate() {
            let at = offset + i as u64;
            sectors
                .entry(at / SECTOR)
                .or_insert_with(|| vec![0; SECTOR as usize])[(at % SECTOR) as usize] = byte;
        }
        Ok(())
    }
    fn flush(&self) -> fs_core::Result<()> {
        if self.armed.load(Ordering::SeqCst) {
            self.writes.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}

/// A freshly formatted volume, armed so that any later write is counted.
fn formatted() -> Arc<Sparse> {
    let dev = Sparse::new();
    fs_xfs::mkfs::format(dev.as_ref(), &fs_xfs::mkfs::Options::default()).expect("mkfs");
    dev.armed.store(true, Ordering::SeqCst);
    dev
}

fn mount(dev: &Arc<Sparse>) -> Filesystem {
    Filesystem::mount(dev.clone() as Arc<dyn BlockRead>).expect("mount")
}

/// Set `bits` in the primary superblock's 32-bit field at `offset`, and
/// stamp a checksum that covers the change, so the edit is the feature it
/// names and not a checksum failure.
fn set_sb_bits(dev: &Arc<Sparse>, offset: usize, bits: u32) {
    dev.armed.store(false, Ordering::SeqCst);
    let mut sb = vec![0u8; SECTOR as usize];
    dev.read_at(0, &mut sb).unwrap();
    let was = u32::from_be_bytes(sb[offset..offset + 4].try_into().unwrap());
    sb[offset..offset + 4].copy_from_slice(&(was | bits).to_be_bytes());
    let crc_at = offsets::CRC;
    sb[crc_at..crc_at + 4].fill(0);
    let crc = crc32c::crc32c(&sb);
    sb[crc_at..crc_at + 4].copy_from_slice(&crc.to_le_bytes());
    dev.write_at(0, &sb).unwrap();
    dev.armed.store(true, Ordering::SeqCst);
}

use fs_xfs::superblock::offsets;
const SB_FEATURES_RO_COMPAT: usize = offsets::FEATURES_RO_COMPAT;
const SB_FEATURES_INCOMPAT: usize = offsets::FEATURES_INCOMPAT;
const SB_FEATURES_LOG_INCOMPAT: usize = offsets::FEATURES_LOG_INCOMPAT;

#[test]
fn a_clean_volume_plans_nothing_and_writes_nothing() {
    let dev = formatted();
    let fs = mount(&dev);
    let plan = repair::plan(&fs, &Exclusive::asserted_by_caller());
    assert_eq!(plan.status, Status::Ready, "{plan:?}");
    assert!(plan.changes.is_empty(), "{plan:?}");
    assert!(plan.refusals.is_empty(), "{plan:?}");
    assert!(plan.unplanned.is_empty(), "{plan:?}");
    assert!(plan.check.is_clean(), "{:?}", plan.check);
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0, "planning wrote");
}

#[test]
fn the_same_volume_gives_the_same_plan() {
    let dev = formatted();
    let fs = mount(&dev);
    let rule = Scribble;
    let a = repair::plan_with(&fs, &Exclusive::asserted_by_caller(), &[&rule]);
    let b = repair::plan_with(&fs, &Exclusive::asserted_by_caller(), &[&rule]);
    assert_eq!(format!("{a:?}"), format!("{b:?}"));
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0, "planning wrote");
}

/// A rule that proposes three changes out of order, one of them a change
/// to what is already there.
struct Scribble;

impl Rule for Scribble {
    fn name(&self) -> &'static str {
        "scribble"
    }
    fn repairs(&self) -> &'static [Code] {
        &[Code::CounterAgfFreeblks]
    }
    fn propose(
        &self,
        fs: &Filesystem,
        _report: &Report,
        proposal: &mut Proposal,
    ) -> fs_xfs::Result<()> {
        let bs = u64::from(fs.superblock().blocksize);
        proposal.put(9 * bs, vec![0xa5; 16], Code::CounterAgfFreeblks, "nine")?;
        proposal.put(8 * bs, vec![0x5a; 16], Code::CounterAgfFreeblks, "eight")?;
        let mut same = vec![0u8; 4];
        fs.device().read_at(0, &mut same)?;
        proposal.put(0, same, Code::CounterAgfFreeblks, "no change")?;
        Ok(())
    }
}

#[test]
fn proposed_changes_are_in_device_order_carry_both_images_and_drop_no_ops() {
    let dev = formatted();
    let fs = mount(&dev);
    let plan = repair::plan_with(&fs, &Exclusive::asserted_by_caller(), &[&Scribble]);
    assert_eq!(plan.status, Status::Ready, "{plan:?}");
    let bs = u64::from(fs.superblock().blocksize);
    let at: Vec<u64> = plan.changes.iter().map(|c| c.offset).collect();
    assert_eq!(at, vec![8 * bs, 9 * bs], "{plan:?}");
    for change in &plan.changes {
        let mut now = vec![0u8; change.before.len()];
        dev.read_at(change.offset, &mut now).unwrap();
        assert_eq!(change.before, now, "before is not what the device holds");
        assert_eq!(change.rule, "scribble");
        assert_eq!(change.code, Code::CounterAgfFreeblks);
    }
    assert_eq!(dev.writes.load(Ordering::SeqCst), 0, "planning wrote");
}

#[test]
fn a_rule_that_proposes_on_a_refused_volume_is_never_asked() {
    let dev = formatted();
    set_sb_bits(
        &dev,
        SB_FEATURES_RO_COMPAT,
        fs_xfs::superblock::ro_compat::RMAPBT,
    );
    let fs = mount(&dev);
    let plan = repair::plan_with(&fs, &Exclusive::asserted_by_caller(), &[&Scribble]);
    assert_eq!(plan.status, Status::Refused, "{plan:?}");
    assert!(plan.changes.is_empty(), "{plan:?}");
}

/// Each feature the planner does not reason about is refused by name.
#[test]
fn a_feature_outside_the_planners_set_is_refused_by_field() {
    use fs_xfs::superblock::{incompat, ro_compat};
    let cases: &[(&str, usize, u32, &str)] = &[
        (
            "rmapbt",
            SB_FEATURES_RO_COMPAT,
            ro_compat::RMAPBT,
            "sb_features_ro_compat",
        ),
        (
            "unknown ro_compat",
            SB_FEATURES_RO_COMPAT,
            1 << 20,
            "sb_features_ro_compat",
        ),
        (
            "exchrange",
            SB_FEATURES_INCOMPAT,
            incompat::EXCHRANGE,
            "sb_features_incompat",
        ),
        (
            "parent",
            SB_FEATURES_INCOMPAT,
            incompat::PARENT,
            "sb_features_incompat",
        ),
        (
            "log_incompat",
            SB_FEATURES_LOG_INCOMPAT,
            1,
            "sb_features_log_incompat",
        ),
    ];
    for &(name, offset, bits, field) in cases {
        let dev = formatted();
        set_sb_bits(&dev, offset, bits);
        let fs = mount(&dev);
        let plan = repair::plan(&fs, &Exclusive::asserted_by_caller());
        assert_eq!(plan.status, Status::Refused, "{name}: {plan:?}");
        assert!(
            plan.refusals
                .iter()
                .any(|f| f.code == Code::RepairFeature && f.location.field == Some(field)),
            "{name}: no repair.feature on {field}: {plan:?}"
        );
        assert_eq!(
            dev.writes.load(Ordering::SeqCst),
            0,
            "{name}: planning wrote"
        );
    }
}

#[test]
fn refusal_codes_are_named_in_the_output_schema() {
    for code in [
        Code::RepairNotExclusive,
        Code::RepairFeature,
        Code::RepairLogDirty,
        Code::RepairIncomplete,
        Code::RepairAmbiguous,
    ] {
        assert!(Code::ALL.contains(&code), "{code:?} is not in Code::ALL");
        assert!(
            code.as_str().starts_with("repair."),
            "{code:?} is {}",
            code.as_str()
        );
    }
}

#[test]
fn a_file_another_holder_has_locked_cannot_be_claimed() {
    let dir = std::env::temp_dir().join(format!("repair-plan-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("held.img");
    let held = std::fs::File::create(&path).unwrap();
    held.lock().expect("the test takes the lock first");
    let refused = Exclusive::claim(&path).expect_err("a held file was claimed");
    assert_eq!(refused.code, Code::RepairNotExclusive, "{refused:?}");
    drop(held);
    Exclusive::claim(&path).expect("a released file can be claimed");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A second rule that wants the bytes `Scribble` already proposed.
struct Rival;

impl Rule for Rival {
    fn name(&self) -> &'static str {
        "rival"
    }
    fn repairs(&self) -> &'static [Code] {
        &[Code::CounterAgiInodes]
    }
    fn propose(
        &self,
        fs: &Filesystem,
        _report: &Report,
        proposal: &mut Proposal,
    ) -> fs_xfs::Result<()> {
        let bs = u64::from(fs.superblock().blocksize);
        proposal.put(
            8 * bs + 8,
            vec![0x11; 4],
            Code::CounterAgiInodes,
            "inside eight",
        )
    }
}

#[test]
fn two_rules_proposing_the_same_bytes_refuse_the_whole_plan() {
    let dev = formatted();
    let fs = mount(&dev);
    let plan = repair::plan_with(&fs, &Exclusive::asserted_by_caller(), &[&Scribble, &Rival]);
    assert_eq!(plan.status, Status::Refused, "{plan:?}");
    assert!(
        plan.changes.is_empty(),
        "a refused plan kept changes: {plan:?}"
    );
    assert!(
        plan.refusals
            .iter()
            .any(|f| f.code == Code::RepairIncomplete && f.what.contains("rival")),
        "{plan:?}"
    );
}
