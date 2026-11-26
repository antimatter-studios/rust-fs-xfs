//! The kill loops' window follows how long the work takes (#628).

#[path = "common/kill_window.rs"]
mod kill_window;

use std::time::Duration;

use kill_window::KillWindow;

fn millis(n: u64) -> Duration {
    Duration::from_millis(n)
}

#[test]
fn a_finished_run_pulls_the_window_halfway_to_when_it_was_found_finished() {
    let mut window = KillWindow::new(millis(715));
    window.finished_within(millis(315));
    assert_eq!(window.millis(), 515);
}

#[test]
fn the_window_never_grows_and_never_empties() {
    let mut window = KillWindow::new(millis(100));
    window.finished_within(millis(900));
    assert_eq!(window.millis(), 100);
    for _ in 0..64 {
        window.finished_within(millis(0));
    }
    assert_eq!(window.millis(), 1);
    assert_eq!(window.delay(u64::MAX), millis(0));
}

#[test]
fn a_kill_point_is_inside_the_window() {
    let window = KillWindow::new(millis(715));
    let mut seed = 0x581_u64;
    for _ in 0..1000 {
        seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
        assert!(window.delay(seed) < millis(715));
    }
}

/// Run 45680871562: the work in the loop took less than half of the 715 ms
/// window, and only 87 of 200 runs were still going at their kill point.
#[test]
fn runs_faster_than_every_timed_run_are_still_killed_more_often_than_not() {
    const ATTEMPTS: usize = 200;
    let work = millis(315);
    for start in 0..100_u64 {
        let mut window = KillWindow::new(millis(715));
        let mut seed = start ^ 0x581;
        let mut killed = 0;
        for _ in 0..ATTEMPTS {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            let delay = window.delay(seed);
            if delay < work {
                killed += 1;
            } else {
                window.finished_within(delay);
            }
        }
        assert!(
            killed >= ATTEMPTS * 3 / 4,
            "seed {start}: only {killed}/{ATTEMPTS} runs were still going at the kill point"
        );
    }
}
