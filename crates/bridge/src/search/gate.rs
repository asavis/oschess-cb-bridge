//! A seam for tests: the next searches on a database wait at their start,
//! each already the newest in its stream but holding no worker and no memory,
//! until the test lets them go. The test can then act while those searches
//! are surely still running. Nothing holds searches outside tests.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Default)]
pub struct Gate {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Searches still to be held when they start.
    to_hold: usize,
    /// Searches held since the gate was shut.
    arrived: usize,
    shut: bool,
}

/// Keeps the gate shut; the held searches go on when it is dropped.
pub struct Holding<'a>(&'a Gate);

impl Gate {
    /// Holds the next `searches` searches to start until the result is dropped.
    pub fn hold(&self, searches: usize) -> Holding<'_> {
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State { to_hold: searches, arrived: 0, shut: true };
        Holding(self)
    }

    /// Where every search starts: one to be held waits here until let go.
    pub(super) fn enter(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.to_hold == 0 {
            return;
        }
        state.to_hold -= 1;
        state.arrived += 1;
        self.changed.notify_all();
        while state.shut {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }
}

impl Holding<'_> {
    /// Whether `searches` searches have been held, waiting at most `limit`.
    pub fn arrived(&self, searches: usize, limit: Duration) -> bool {
        let state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        let (state, _) = self
            .0
            .changed
            .wait_timeout_while(state, limit, |s| s.arrived < searches)
            .unwrap_or_else(|e| e.into_inner());
        state.arrived >= searches
    }
}

impl Drop for Holding<'_> {
    fn drop(&mut self) {
        let mut state = self.0.state.lock().unwrap_or_else(|e| e.into_inner());
        state.to_hold = 0;
        state.shut = false;
        self.0.changed.notify_all();
    }
}
