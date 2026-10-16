//! What the event loop is running, and when it stops answering (#332). The
//! tray, the menu and every synchronous command run on the event loop's
//! thread, and while one of them waits the whole tray is dead to the user.
//! Each handler marks its name while it runs ([`BUSY`]), and a watchdog thread
//! asks the event loop for a beat every few seconds ([`Watch`]): the log then
//! names what it was running when it stopped answering.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long the event loop may stay silent before the log says so.
pub const STALL: Duration = Duration::from_secs(15);

/// What the event loop runs now: one handler at a time, since it runs one.
pub static BUSY: Busy = Busy { now: Mutex::new(None) };

pub struct Busy {
    now: Mutex<Option<(String, Instant)>>,
}

impl Busy {
    /// Marks `what` as running until the mark is dropped, which restores what
    /// ran before: a handler may call another.
    pub fn mark(&self, what: impl Into<String>) -> Mark<'_> {
        let before = self.lock().replace((what.into(), Instant::now()));
        Mark { busy: self, before }
    }

    /// What runs now, and since when.
    pub fn current(&self) -> Option<(String, Instant)> {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<(String, Instant)>> {
        self.now.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A running handler's mark; see [`Busy::mark`].
pub struct Mark<'a> {
    busy: &'a Busy,
    before: Option<(String, Instant)>,
}

impl Drop for Mark<'_> {
    fn drop(&mut self) {
        *self.busy.lock() = self.before.take();
    }
}

/// What the watchdog has to log.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// No beat for `silent`: logged once per stall.
    Stopped { silent: Duration },
    /// The beats came back after a stall that lasted `after`.
    Answered { after: Duration },
}

/// The watchdog's reading of the beats.
#[derive(Default)]
pub struct Watch {
    /// The last beat before the current stall, once the stall was logged.
    stalled_after: Option<Instant>,
    /// When the watchdog last read the beats.
    last_check: Option<Instant>,
    /// When the watchdog last woke from a sleep as long as a stall.
    resumed: Option<Instant>,
}

impl Watch {
    /// Reads `last_beat`, the time the event loop last answered, at `now`.
    pub fn check(&mut self, last_beat: Instant, now: Instant) -> Option<Event> {
        // The watchdog itself slept as long as a stall: the computer was
        // asleep or starved, not the event loop alone. Silence counts from now.
        if self.last_check.replace(now).is_some_and(|before| now.saturating_duration_since(before) >= STALL) {
            self.resumed = Some(now);
        }
        let since = self.resumed.map_or(last_beat, |resumed| resumed.max(last_beat));
        let silent = now.saturating_duration_since(since);
        match self.stalled_after {
            None if silent >= STALL => {
                self.stalled_after = Some(last_beat);
                Some(Event::Stopped { silent })
            }
            Some(before) if last_beat > before => {
                self.stalled_after = None;
                Some(Event::Answered { after: last_beat - before })
            }
            _ => None,
        }
    }
}

/// The log line for a stall seen at `now`, naming what ran then.
pub fn stopped_line(silent: Duration, running: Option<(String, Instant)>, now: Instant) -> String {
    let running = match running {
        Some((what, since)) => {
            format!("it has been running {what} for {} s", now.saturating_duration_since(since).as_secs())
        }
        None => "it was running none of the app's handlers".to_string(),
    };
    format!("the event loop has not answered for {} s; {running}", silent.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mark_restores_what_ran_before() {
        let busy = Busy { now: Mutex::new(None) };
        {
            let _menu = busy.mark("the menu item settings");
            {
                let _command = busy.mark("the command settings");
                assert_eq!(busy.current().unwrap().0, "the command settings");
            }
            assert_eq!(busy.current().unwrap().0, "the menu item settings");
        }
        assert_eq!(busy.current(), None);
    }

    #[test]
    fn a_stall_is_logged_once_and_its_end_with_its_length() {
        let start = Instant::now();
        let mut watch = Watch::default();
        // Read every two seconds, as the watchdog does.
        let mut read = |beat: Instant, seconds: u64| {
            let events: Vec<Event> =
                (0..=seconds).step_by(2).filter_map(|s| watch.check(beat, beat + Duration::from_secs(s))).collect();
            events
        };
        assert_eq!(
            read(start, 40),
            [Event::Stopped { silent: STALL + Duration::from_secs(1) }],
            "logged once per stall"
        );
        let answered = start + Duration::from_secs(42);
        assert_eq!(read(answered, 0), [Event::Answered { after: Duration::from_secs(42) }]);
        assert_eq!(read(answered, 10), []);
        // A later stall is logged again.
        assert!(matches!(read(answered, 20)[..], [Event::Stopped { .. }]));
    }

    #[test]
    fn a_computer_that_slept_is_no_stall() {
        let start = Instant::now();
        let mut watch = Watch::default();
        assert_eq!(watch.check(start, start), None);
        let woke = start + Duration::from_secs(3600);
        assert_eq!(watch.check(start, woke), None, "the watchdog slept as long as a stall");
        assert_eq!(watch.check(start, woke + STALL / 2), None);
        // An event loop that stays silent after the computer woke is a stall.
        assert_eq!(watch.check(start, woke + STALL), Some(Event::Stopped { silent: STALL }));
    }

    #[test]
    fn the_stall_line_names_what_ran() {
        let now = Instant::now();
        let running = Some(("the command settings".to_string(), now - Duration::from_secs(16)));
        assert_eq!(
            stopped_line(STALL, running, now),
            "the event loop has not answered for 15 s; it has been running the command settings for 16 s"
        );
        assert_eq!(
            stopped_line(STALL, None, now),
            "the event loop has not answered for 15 s; it was running none of the app's handlers"
        );
    }
}
