//! Opening a window, followed stage by stage (#332). A window is *requested*
//! when its build starts, *built* when Tauri returns it, and *ready* when its
//! page says it has drawn what it shows first. A watcher asks
//! [`Openings::overdue`] once [`LIMIT`] has passed: a window that never got
//! ready is reported with the stage it stopped at. Before, such a click did
//! nothing and left nothing in the log.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a window may take from its request to a drawn page.
pub const LIMIT: Duration = Duration::from_secs(10);

/// How far an opening got.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Requested,
    Built,
    Ready,
}

impl Stage {
    /// The stage's name in the log.
    pub fn name(self) -> &'static str {
        match self {
            Stage::Requested => "requested",
            Stage::Built => "built",
            Stage::Ready => "ready",
        }
    }
}

/// The latest opening of each window, by its label.
#[derive(Default)]
pub struct Openings {
    latest: Mutex<HashMap<String, Opening>>,
}

#[derive(Clone, Copy, Debug)]
struct Opening {
    attempt: u64,
    stage: Stage,
    since: Instant,
}

impl Openings {
    /// Starts an opening of `label`'s window at `now`, in place of any before;
    /// the attempt it returns names this one.
    pub fn request(&self, label: &str, now: Instant) -> u64 {
        let mut latest = self.lock();
        let attempt = latest.get(label).map_or(1, |o| o.attempt + 1);
        latest.insert(label.to_string(), Opening { attempt, stage: Stage::Requested, since: now });
        attempt
    }

    /// Tauri returned `attempt`'s window. A page that said it was ready before
    /// Tauri returned stays ready.
    pub fn built(&self, label: &str, attempt: u64) {
        if let Some(o) = self.lock().get_mut(label).filter(|o| o.attempt == attempt && o.stage == Stage::Requested) {
            o.stage = Stage::Built;
        }
    }

    /// `label`'s page has drawn. A page does not know its attempt; it is the
    /// latest one's, since a window is built again only after the one before
    /// it is gone.
    pub fn ready(&self, label: &str) {
        if let Some(o) = self.lock().get_mut(label) {
            o.stage = Stage::Ready;
        }
    }

    /// `attempt`'s build failed and was reported then: no watcher reports it
    /// again.
    pub fn failed(&self, label: &str, attempt: u64) {
        let mut latest = self.lock();
        if latest.get(label).is_some_and(|o| o.attempt == attempt) {
            latest.remove(label);
        }
    }

    /// `label`'s window is gone, closed by the user or destroyed by the app.
    /// An opening still at *requested* belongs to the next window, built
    /// once this one was gone, and stays.
    pub fn closed(&self, label: &str) {
        let mut latest = self.lock();
        if latest.get(label).is_some_and(|o| o.stage != Stage::Requested) {
            latest.remove(label);
        }
    }

    /// The stage `attempt` stopped at, when it is still `label`'s latest
    /// opening and its page is not ready; asked once [`LIMIT`] has passed.
    pub fn overdue(&self, label: &str, attempt: u64) -> Option<Stage> {
        self.lock().get(label).filter(|o| o.attempt == attempt && o.stage != Stage::Ready).map(|o| o.stage)
    }

    /// Whether `label`'s existing window can be brought forward at `now`: its
    /// page is ready, or it is still within [`LIMIT`] of its request. A window
    /// with no opening on record, built before this was kept, can be.
    pub fn usable(&self, label: &str, now: Instant) -> bool {
        self.lock().get(label).is_none_or(|o| o.stage == Stage::Ready || now.saturating_duration_since(o.since) < LIMIT)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Opening>> {
        self.latest.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_that_gets_ready_is_not_overdue() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now());
        openings.built("settings", attempt);
        openings.ready("settings");
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    #[test]
    fn an_opening_is_overdue_at_the_stage_it_stopped_at() {
        let openings = Openings::default();
        let never_built = openings.request("settings", Instant::now());
        assert_eq!(openings.overdue("settings", never_built), Some(Stage::Requested));
        let never_ready = openings.request("first-run", Instant::now());
        openings.built("first-run", never_ready);
        assert_eq!(openings.overdue("first-run", never_ready), Some(Stage::Built));
    }

    #[test]
    fn a_page_ready_before_tauri_returned_stays_ready() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now());
        openings.ready("settings");
        openings.built("settings", attempt);
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    #[test]
    fn a_later_opening_replaces_the_one_before() {
        let openings = Openings::default();
        let first = openings.request("settings", Instant::now());
        let second = openings.request("settings", Instant::now());
        assert_ne!(first, second);
        // The first one's watcher finds nothing to report: the second is watched.
        assert_eq!(openings.overdue("settings", first), None);
        assert_eq!(openings.overdue("settings", second), Some(Stage::Requested));
        // Tauri returning the first window does not move the second opening.
        openings.built("settings", first);
        assert_eq!(openings.overdue("settings", second), Some(Stage::Requested));
    }

    #[test]
    fn a_failed_build_is_not_reported_twice() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now());
        openings.failed("settings", attempt);
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    #[test]
    fn a_window_closed_before_its_page_was_ready_is_not_reported() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now());
        openings.built("settings", attempt);
        openings.closed("settings");
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    /// The window destroyed to be built again may report its end after the
    /// next build was requested: that request stays watched.
    #[test]
    fn closing_the_window_before_keeps_the_next_request() {
        let openings = Openings::default();
        let next = openings.request("settings", Instant::now());
        openings.closed("settings");
        assert_eq!(openings.overdue("settings", next), Some(Stage::Requested));
    }

    #[test]
    fn a_window_whose_page_never_got_ready_is_not_brought_forward() {
        let openings = Openings::default();
        let start = Instant::now();
        assert!(openings.usable("settings", start), "a window with no opening on record");
        let attempt = openings.request("settings", start);
        openings.built("settings", attempt);
        assert!(openings.usable("settings", start + LIMIT / 2), "still loading");
        assert!(!openings.usable("settings", start + LIMIT), "stopped at built");
        openings.ready("settings");
        assert!(openings.usable("settings", start + LIMIT * 10));
    }
}
