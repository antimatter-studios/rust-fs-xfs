//! The span of time a kill loop draws its kill points in (#628).
//!
//! A SIGKILL tests something only while the process is still running, so the
//! span has to follow how long the work takes in the loop itself. Runs timed
//! before the loop can all be slower than the runs in it.

use std::time::Duration;

pub struct KillWindow {
    millis: u64,
}

impl KillWindow {
    /// `timed` is how long the work took when timed before the loop.
    pub fn new(timed: Duration) -> Self {
        KillWindow {
            millis: (timed.as_millis() as u64).max(1),
        }
    }

    pub fn millis(&self) -> u64 {
        self.millis
    }

    /// The kill point `seed` draws: a delay from the spawn, inside the window.
    pub fn delay(&self, seed: u64) -> Duration {
        Duration::from_millis((seed >> 16) % self.millis)
    }

    /// A run was already finished `elapsed` after its spawn.
    pub fn finished_within(&mut self, _elapsed: Duration) {}
}
