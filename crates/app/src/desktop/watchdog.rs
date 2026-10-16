//! The event loop's watchdog (#332, `crate::busy`): a thread asks the event
//! loop for a beat every [`BEAT`]. When none has come for `busy::STALL`, it
//! logs what the event loop was running, and once the beats come back, how
//! long the stall lasted.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::AppHandle;

use crate::busy::{self, BUSY, Event, Watch};

/// How often the event loop is asked for a beat.
const BEAT: Duration = Duration::from_secs(2);

pub fn start(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("event-loop-watch".into()).spawn(move || {
        let last = Arc::new(Mutex::new(Instant::now()));
        let asked = Arc::new(AtomicBool::new(false));
        let mut watch = Watch::default();
        loop {
            // One beat asked at a time: a stalled event loop does not pile them up.
            if !asked.swap(true, Ordering::AcqRel) {
                let (beat, answered) = (last.clone(), asked.clone());
                let posted = app.run_on_main_thread(move || {
                    *beat.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
                    answered.store(false, Ordering::Release);
                });
                // The event loop has ended: the app is exiting.
                if posted.is_err() {
                    return;
                }
            }
            std::thread::sleep(BEAT);
            let (beat, now) = (*last.lock().unwrap_or_else(|e| e.into_inner()), Instant::now());
            match watch.check(beat, now) {
                Some(Event::Stopped { silent }) => bridge::log!("{}", busy::stopped_line(silent, BUSY.current(), now)),
                Some(Event::Answered { after }) => {
                    bridge::log!("the event loop answered again after {} s", after.as_secs());
                }
                None => {}
            }
        }
    });
    if let Err(e) = spawned {
        bridge::log!("the event loop is not watched: {e}");
    }
}
