//! The stable-toolchain half of the fuzzing setup: replay the corpus,
//! then mutate it, and refuse if a decoder panics, hangs, or if the
//! suite quietly stopped doing any work.
//!
//! # Why there are two halves
//!
//! `fuzz/` holds `cargo-fuzz` targets. Those are the explorer: they run
//! for as long as you give them, on nightly, and find inputs nobody
//! thought of. They cannot be a required check, because how long they
//! run decides what they find, and a fresh discovery would fail
//! whichever unrelated pull request happened to be open at the time.
//!
//! This suite is the gate. It is deterministic, it runs in under a
//! second on the stable toolchain in every pull request, and it reads
//! the same `fuzz/corpus/` directory the explorer does. Anything the
//! explorer finds gets committed there, which is what makes a finding
//! stay fixed rather than living in somebody's local `fuzz/artifacts`.
//!
//! # Why the corpus is real blocks and not random bytes
//!
//! Every seed under `fuzz/corpus/` was cut out of an image `mkfs.xfs`
//! wrote -- see `scripts/make-fuzz-corpus.sh`, which rebuilds the whole
//! directory in about two seconds. Random bytes are rejected by the
//! magic-number check on the first line of every one of these decoders
//! and never reach the arithmetic underneath. A real block with one
//! field changed reaches all of it.
//!
//! # Why mutation preserves length
//!
//! A hostile image controls what is *in* a block. It does not control
//! how long a block is: the device hands back a full sector or a full
//! filesystem block, and a device that cannot returns `ShortRead`
//! before any of this code is reached. Feeding a three-byte buffer to a
//! decoder that is only ever called with 4096 bytes produces a panic
//! that no image can cause, which is noise rather than a finding. The
//! two decoders that genuinely take a variable-length buffer --
//! `extent::parse_list` and `log_write::log_dinode_from_disk` -- are
//! given varying lengths, because for those it is a real input.
//!
//! # What counts as a failure
//!
//! A panic, which the test harness catches on its own. A hang, which it
//! does not -- so the work runs on a second thread against a deadline,
//! and on expiry this suite names the target, the seed and the case
//! before exiting non-zero. An unbounded walk was one of the
//! 2026-09-06 findings, and it shows up as a hang, not a panic.
//!
//! And doing nothing. The case count is asserted against a floor, for
//! the same reason `scripts/ci-test.sh` asserts one: a suite that
//! stopped generating work would otherwise pass faster than ever.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::time::Duration;

use fs_xfs::{ag, agfl, dir, extent, group_write, inode, log_write, refcount, rmap, Superblock};

/// Distinct starting points for the mutation stream. Fixed, so a
/// failure reproduces from the message alone.
const SEEDS: u64 = 8;

/// Mutated cases per (corpus file, seed) pair.
const CASES_PER_SEED: usize = 256;

/// Below this, the suite is not doing its job. The real number is an
/// order of magnitude higher; this catches a target list or a corpus
/// that has collapsed, not a small change in either.
const CASE_FLOOR: usize = 20_000;

/// Long enough that a slow machine under load is never the reason, short
/// enough that a genuine hang is reported rather than waiting for the
/// job timeout.
const DEADLINE: Duration = Duration::from_secs(120);

// ---------------------------------------------------------------- targets

/// One decoder, and the corpus directory whose blocks are the right
/// shape to feed it.
struct Target {
    /// Matches a directory under `fuzz/corpus/` and, for the targets the
    /// explorer also has, a `[[bin]]` name in `fuzz/Cargo.toml`.
    corpus: &'static str,
    /// Distinguishes two targets that read the same corpus directory.
    name: &'static str,
    /// The length this decoder is called with in the real path: a
    /// sector, a filesystem block, or `VARIABLE` for the two that
    /// genuinely take a buffer of whatever length the caller has.
    ///
    /// Seeds are brought to this length before anything else happens,
    /// which is what keeps this suite and the `cargo-fuzz` target for
    /// the same decoder agreeing about what their shared corpus means.
    /// Without it a short input -- and libFuzzer produces a great many
    /// of them -- would be normalised by the explorer and fed raw to
    /// the decoder here, at a length no device can hand back.
    len: usize,
    run: fn(&[u8]),
}

/// A decoder whose input length is a real variable rather than a
/// property of the device.
const VARIABLE: usize = 0;

const SECTOR: usize = 512;
const BLOCK: usize = 4096;

/// Present a seed as a block of exactly `len`, the way the explorer's
/// `fs_xfs_fuzz::block` does. Short input repeats rather than being
/// zero-padded: zeros are a shape these decoders reject on the first
/// line, and the point is to get further in than that.
fn to_length(seed: &[u8], len: usize) -> Vec<u8> {
    if len == VARIABLE {
        return seed.to_vec();
    }
    if seed.is_empty() {
        return vec![0; len];
    }
    seed.iter().copied().cycle().take(len).collect()
}

fn superblock() -> &'static Superblock {
    static SB: OnceLock<Superblock> = OnceLock::new();
    SB.get_or_init(|| {
        let raw = one_seed("superblock");
        Superblock::parse(&raw).expect(
            "the committed superblock seed must parse -- if this fails the corpus is wrong, \
             not the decoder; rebuild it with scripts/make-fuzz-corpus.sh",
        )
    })
}

fn agf() -> &'static ag::Agf {
    static AGF: OnceLock<ag::Agf> = OnceLock::new();
    AGF.get_or_init(|| {
        let raw = one_seed("agf");
        ag::Agf::parse(&raw, superblock(), 0).expect("the committed AGF seed must parse")
    })
}

fn targets() -> Vec<Target> {
    vec![
        Target {
            corpus: "superblock",
            name: "superblock",
            len: SECTOR,
            run: |b| {
                let _ = Superblock::parse(b);
            },
        },
        Target {
            corpus: "agf",
            name: "agf",
            len: SECTOR,
            run: |b| {
                let _ = ag::Agf::parse(b, superblock(), 0);
            },
        },
        Target {
            corpus: "agi",
            name: "agi",
            len: SECTOR,
            run: |b| {
                let _ = ag::Agi::parse(b, superblock(), 0);
            },
        },
        Target {
            corpus: "agfl",
            name: "agfl",
            len: SECTOR,
            run: |b| {
                let _ = agfl::Agfl::parse(b, superblock(), agf(), 0);
            },
        },
        Target {
            corpus: "inode",
            name: "inode",
            len: SECTOR,
            run: |b| {
                let _ = inode::Inode::parse(b, superblock(), 128);
            },
        },
        Target {
            corpus: "inode",
            name: "log_dinode",
            len: VARIABLE,
            run: |b| {
                let _ = log_write::log_dinode_from_disk(b);
            },
        },
        Target {
            corpus: "dir_data_block",
            name: "dir_data_block",
            len: BLOCK,
            run: |b| {
                let _ = dir::parse_data_block(b, superblock());
                let _ = dir::verify_data_block(b, superblock(), 0, 128);
            },
        },
        Target {
            corpus: "dir_data_block",
            name: "dir_block_form",
            len: BLOCK,
            run: |b| {
                let _ = dir::parse_block_form(b, superblock());
            },
        },
        Target {
            corpus: "dir_leaf",
            name: "dir_leaf",
            len: BLOCK,
            run: |b| {
                let _ = dir::parse_leaf(b, superblock());
                let _ = dir::verify_da_block(b, superblock(), 0, 128);
            },
        },
        Target {
            corpus: "dir_node",
            name: "dir_node",
            len: BLOCK,
            run: |b| {
                let _ = dir::parse_node(b, superblock());
                let _ = dir::verify_da_block(b, superblock(), 0, 128);
            },
        },
        Target {
            corpus: "btree_leaf",
            name: "btree_leaf",
            len: BLOCK,
            run: |b| {
                // The count is what a crafted block controls, so take it
                // from the block the way the read path does rather than
                // inventing one -- including the case where it is a lie.
                for record_bytes in [8usize, 12, 16, 24] {
                    if let Ok(numrecs) = group_write::leaf_numrecs(b, record_bytes) {
                        let _ = group_write::leaf_records(b, numrecs);
                        let _ = rmap::leaf_records(b, numrecs);
                        let _ = refcount::leaf_records(b, numrecs);
                    }
                }
                // And again with a count the block never agreed to, which
                // is what the `min` backstops in those three exist for.
                for numrecs in [0u16, 1, u16::MAX] {
                    let _ = group_write::leaf_records(b, numrecs);
                    let _ = rmap::leaf_records(b, numrecs);
                    let _ = refcount::leaf_records(b, numrecs);
                }
            },
        },
        Target {
            corpus: "bmbt",
            name: "bmbt",
            len: BLOCK,
            run: |b| {
                for record_bytes in [16usize] {
                    if let Ok(numrecs) = group_write::leaf_numrecs(b, record_bytes) {
                        let _ = extent::parse_list(b, u64::from(numrecs));
                    }
                }
            },
        },
        Target {
            corpus: "bmbt",
            name: "extent_list",
            len: VARIABLE,
            run: |b| {
                // `count` is read out of an inode fork in the real path,
                // so it is attacker-controlled and unrelated to how long
                // the buffer actually is. The huge values are the reason
                // the checked_mul in parse_list is there.
                for count in [0u64, 1, 16, 4096, u64::MAX / 16, u64::MAX] {
                    let _ = extent::parse_list(b, count);
                }
            },
        },
    ]
}

// ---------------------------------------------------------------- corpus

fn corpus_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/corpus")
}

/// Every seed in one corpus directory, sorted so the order is the same
/// everywhere.
fn seeds(corpus: &str) -> Vec<(String, Vec<u8>)> {
    let dir = corpus_root().join(corpus);
    let mut out: Vec<(String, Vec<u8>)> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading the corpus directory {}: {e}", dir.display()))
        .map(|entry| {
            let path = entry.expect("corpus directory entry").path();
            let bytes = std::fs::read(&path)
                .unwrap_or_else(|e| panic!("reading the seed {}: {e}", path.display()));
            let name = path
                .file_name()
                .expect("seed file name")
                .to_string_lossy()
                .into_owned();
            (name, bytes)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn one_seed(corpus: &str) -> Vec<u8> {
    seeds(corpus)
        .into_iter()
        .next()
        .unwrap_or_else(|| panic!("the {corpus} corpus is empty"))
        .1
}

// ---------------------------------------------------------------- mutation

/// xorshift64*. Small, deterministic, and not a dependency.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Any non-zero state will do; the constant just keeps seed 0
        // from being a fixed point.
        Rng(seed ^ 0x9e37_79b9_7f4a_7c15)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }
}

/// One mutation of a real block, preserving its length.
///
/// The operations are chosen for what they reach rather than for
/// variety: a single flipped bit finds a boundary check that is off by
/// one, an extreme field value finds arithmetic that overflows, and a
/// swapped pair of words finds a decoder that trusted two fields to be
/// ordered.
fn mutate(seed: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut out = seed.to_vec();
    if out.is_empty() {
        return out;
    }

    match rng.below(5) {
        0 => {
            for _ in 0..=rng.below(8) {
                let at = rng.below(out.len());
                out[at] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            let at = rng.below(out.len());
            let len = 1 + rng.below(16.min(out.len() - at));
            let fill = if rng.next() & 1 == 0 { 0x00 } else { 0xff };
            out[at..at + len].fill(fill);
        }
        2 => {
            let width = [2usize, 4, 8][rng.below(3)];
            if out.len() >= width {
                let at = rng.below(out.len() - width + 1) & !(width - 1);
                let value: u64 = match rng.below(4) {
                    0 => 0,
                    1 => 1,
                    2 => u64::MAX,
                    _ => rng.next(),
                };
                out[at..at + width].copy_from_slice(&value.to_be_bytes()[8 - width..]);
            }
        }
        3 => {
            if out.len() >= 8 {
                let a = rng.below(out.len() / 4) * 4;
                let b = rng.below(out.len() / 4) * 4;
                if a + 4 <= out.len() && b + 4 <= out.len() {
                    for i in 0..4 {
                        out.swap(a + i, b + i);
                    }
                }
            }
        }
        _ => {
            if out.len() >= 4 {
                let at = rng.below(out.len() / 4) * 4;
                let word = u32::from_be_bytes(out[at..at + 4].try_into().expect("4 bytes"));
                let delta = [1i64, -1, 2, -2, 255, -255][rng.below(6)];
                let changed = (i64::from(word).wrapping_add(delta)) as u32;
                out[at..at + 4].copy_from_slice(&changed.to_be_bytes());
            }
        }
    }
    out
}

/// The case in flight, readable even if the lock was poisoned by the
/// panic we are trying to describe.
fn describe(current: &Arc<Mutex<String>>) -> String {
    match current.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

// ---------------------------------------------------------------- tests

#[test]
fn every_target_has_a_corpus() {
    for target in targets() {
        let found = seeds(target.corpus);
        assert!(
            !found.is_empty(),
            "the target {} reads fuzz/corpus/{}, which holds no seeds -- a target with an \
             empty corpus runs no cases and would pass in silence. Rebuild the corpus with \
             scripts/make-fuzz-corpus.sh",
            target.name,
            target.corpus,
        );
    }
}

#[test]
fn the_corpus_replays_exactly_as_committed() {
    // Before any mutation: every seed, byte for byte. This is the half
    // that keeps a fixed finding fixed, so it is a test on its own
    // rather than the first iteration of the mutation loop.
    let mut replayed = 0usize;
    for target in targets() {
        for (name, bytes) in seeds(target.corpus) {
            eprintln!("replaying {}/{}", target.corpus, name);
            (target.run)(&to_length(&bytes, target.len));
            replayed += 1;
        }
    }
    assert!(
        replayed >= 16,
        "only {replayed} corpus files were replayed; the corpus has shrunk",
    );
}

#[test]
fn deterministic_mutations_of_real_blocks_are_survived() {
    let cases = Arc::new(AtomicUsize::new(0));
    let current = Arc::new(Mutex::new(String::from("(not started)")));
    let (done_tx, done_rx) = mpsc::channel();

    // A panic arrives with no clue which of forty thousand cases caused
    // it, because the case is a local in another thread by the time the
    // message is printed. The hook prints the one that was in flight,
    // which is the whole reproduction recipe: target, seed file,
    // starting point and case number.
    let hook_current = Arc::clone(&current);
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        eprintln!("\nfuzz gate: panicked at {}", describe(&hook_current));
        previous_hook(info);
    }));

    let worker_cases = Arc::clone(&cases);
    let worker_current = Arc::clone(&current);
    let worker = std::thread::spawn(move || {
        for target in targets() {
            for (seed_name, bytes) in seeds(target.corpus) {
                let base = to_length(&bytes, target.len);
                for seed in 0..SEEDS {
                    let mut rng = Rng::new(seed);
                    for case in 0..CASES_PER_SEED {
                        *worker_current.lock().expect("progress lock") =
                            format!("{} / {seed_name} / seed {seed} / case {case}", target.name);
                        let mutated = mutate(&base, &mut rng);
                        (target.run)(&mutated);
                        worker_cases.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        let _ = done_tx.send(());
    });

    // Two different failures arrive on this channel and they must not be
    // confused. A timeout means the worker is still running and has not
    // finished: a hang. A disconnect means the sender was dropped
    // without sending: the worker panicked, and the panic is the thing
    // worth reporting, not a deadline that never expired.
    match done_rx.recv_timeout(DEADLINE) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            // Fall through to the join below, which turns the worker's
            // panic into this test's failure.
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The worker cannot be killed, so the process has to go. A
            // hang is a real defect -- a walk with no visit budget looks
            // exactly like this -- and it has to be reported rather than
            // left to the CI job timeout, which says nothing about where
            // it was.
            // Written to the process's stderr rather than through
            // `eprintln!`, which the test harness captures into a buffer
            // it only prints when a test finishes. Exiting here means it
            // never finishes, and the one message explaining why would
            // be thrown away with the buffer.
            let _ = writeln!(
                std::io::stderr(),
                "\nhung: no progress for {:?} at {}\n\
                 A decoder did not return. Reproduce by running this target against that \
                 seed with the same starting point.",
                DEADLINE,
                describe(&current),
            );
            let _ = std::io::stderr().flush();
            std::process::exit(1);
        }
    }

    let outcome = worker.join();
    // Put the ordinary hook back before failing, so the message below is
    // not prefixed by the same case a second time.
    let _ = std::panic::take_hook();
    if outcome.is_err() {
        // The hook has already printed the case and the panic itself.
        // This turns it into a test failure rather than a stray message
        // from a thread nobody was watching.
        panic!("a decoder panicked at {}", describe(&current));
    }

    let total = cases.load(Ordering::Relaxed);
    assert!(
        total >= CASE_FLOOR,
        "only {total} mutated cases ran, below the floor of {CASE_FLOOR} -- the target list \
         or the corpus has collapsed, and a suite that runs nothing passes quickly",
    );
    eprintln!("{total} mutated cases");
}

#[test]
fn the_gate_covers_every_explorer_target() {
    // The two tiers drift apart the moment somebody adds a cargo-fuzz
    // target and forgets that nothing gates it on the stable toolchain.
    let manifest =
        std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
            .expect("reading fuzz/Cargo.toml");

    let explorer: Vec<String> = manifest
        .lines()
        .filter_map(|line| line.strip_prefix("name = \""))
        .filter_map(|rest| rest.strip_suffix('"'))
        .map(str::to_owned)
        // The package name is the first `name =` in the file.
        .skip(1)
        .collect();

    assert!(
        !explorer.is_empty(),
        "fuzz/Cargo.toml declares no [[bin]] targets",
    );

    let gated: Vec<&str> = targets().iter().map(|t| t.name).collect();
    for name in &explorer {
        assert!(
            gated.contains(&name.as_str()),
            "fuzz/fuzz_targets/{name}.rs has no counterpart in this suite, so nothing replays \
             its corpus on the stable toolchain and anything it finds would only stay fixed \
             for as long as somebody keeps running the fuzzer by hand",
        );
    }
}
