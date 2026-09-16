//! `encode_record`'s panic is a precondition a caller outside the crate
//! can check first (#131).
//!
//! `encode_record` is public and asserts, in release, that a payload pads
//! to no more basic blocks than one record header describes. The function
//! that computes that limit, `max_payload`, was private, so a caller could
//! build a `Placement` easily and a safe call not at all. This file is in
//! `tests/`, outside the crate: that it compiles is half the assertion.

use fs_xfs::format::log_items::rec_header::XLOG_CYCLE_DATA_ENTRIES;
use fs_xfs::log_write::{encode_record, max_payload, Placement};

const BBSIZE: usize = 512;

fn placement(iclog_size: u32) -> Placement {
    Placement {
        block: 0,
        cycle: 1,
        prev_block: u32::MAX,
        tail_lsn: 0,
        uuid: [0xAB; 16],
        iclog_size,
    }
}

#[test]
fn a_payload_within_max_payload_never_reaches_the_panic() {
    let iclog = 32 * 1024;
    let cap = max_payload(iclog).expect("a 32 KiB record is describable");
    assert!(cap.div_ceil(BBSIZE) <= XLOG_CYCLE_DATA_ENTRIES);
    let bytes = encode_record(&placement(iclog), 1, &vec![0x5A; cap]);
    assert_eq!(bytes.len(), BBSIZE + cap.div_ceil(BBSIZE) * BBSIZE);
}

#[test]
fn a_payload_past_the_documented_limit_panics_and_the_limit_says_so() {
    let too_big = (XLOG_CYCLE_DATA_ENTRIES + 1) * BBSIZE;
    let panicked =
        std::panic::catch_unwind(|| encode_record(&placement(32 * 1024), 1, &vec![0; too_big]));
    assert!(panicked.is_err(), "the documented panic did not happen");
    // And a caller asking first would not have made that call: no record
    // size this writer accepts admits it.
    for iclog in [4 * 1024u32, 16 * 1024, 32 * 1024] {
        assert!(max_payload(iclog).unwrap() < too_big, "{iclog}");
    }
    assert!(
        max_payload(64 * 1024).is_err(),
        "a multi-block header is refused up front"
    );
}
