//! Sending the failure reports (#333, `crate::reports`). [`file`] puts a
//! report in the outbox and wakes the sender, a thread of its own that posts
//! it with the system's `curl.exe`. Neither the event loop nor WebView2 takes
//! part, so a report leaves when they are what failed. A failure
//! `bridge::fault!` raises (#342) waits in [`FAULTS`] and is filed by the
//! sender itself.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

use super::{alerts, channel, shared, system};
use crate::diagnostics;
use crate::reports::{self, Context, Dsn, Kind, Outbox, Report, Transport};

/// How long the sender sleeps when nothing waits; a report filed meanwhile
/// wakes it.
const IDLE: Duration = Duration::from_secs(3600);

struct Reporter {
    app: AppHandle,
    dsn: Dsn,
    data: PathBuf,
    started: Instant,
    /// Set when a report is filed; the sender waits on it.
    filed: (Mutex<bool>, Condvar),
    /// One report is filed at a time: the outbox and its record of the day.
    filing: Mutex<()>,
}

static REPORTER: OnceLock<Reporter> = OnceLock::new();

/// The failures `bridge::fault!` raised that wait for the sender. A failure
/// can be raised under a lock that a report's context takes, such as the
/// status the view reads, so the thread that raised it only queues it, and
/// the sender files it holding nothing.
static FAULTS: Mutex<VecDeque<Report>> = Mutex::new(VecDeque::new());

/// The most failures that wait; more are logged and not reported. One failure
/// goes once a day per version, so only many different ones fill it.
const MAX_FAULTS: usize = 32;

thread_local! {
    /// This thread is filing a report: a panic while it does reports nothing,
    /// where it would wait on itself.
    static FILING: Cell<bool> = const { Cell::new(false) };
}

/// Starts the sender, when this build has a DSN and the bridge a data folder.
/// What waits from an earlier run goes first.
pub fn start(app: &AppHandle) {
    let Some(dsn) = Dsn::parse(reports::DSN) else { return };
    let Ok(data) = shared(app).dir() else { return };
    let reporter = Reporter {
        app: app.clone(),
        dsn,
        data,
        started: Instant::now(),
        filed: (Mutex::new(false), Condvar::new()),
        filing: Mutex::new(()),
    };
    if REPORTER.set(reporter).is_err() {
        return;
    }
    bridge::log::on_fault(raise);
    if let Err(e) = std::thread::Builder::new().name("reports".into()).spawn(send) {
        bridge::log!("report: no sender: {e}");
    }
}

/// Whether this build sends reports at all.
pub fn available() -> bool {
    Dsn::parse(reports::DSN).is_some()
}

/// The user turned the reports off: the ones waiting go no more, but those
/// sent by hand.
pub fn turned_off(app: &AppHandle) {
    let Ok(data) = shared(app).dir() else { return };
    // After any filing under way: what it filed is dropped as well.
    let _filing = REPORTER.get().map(|r| r.filing.lock().unwrap_or_else(|e| e.into_inner()));
    let dropped = Outbox::in_data(&data).drop_automatic();
    if dropped > 0 {
        bridge::log!("report: turned off, so {dropped} waiting reports are dropped");
    }
}

/// Files `report` and wakes the sender. Answers the code a user can quote:
/// the new report's, or that of the report the same failure went as today.
/// `None` when nothing goes: this build has no DSN, or the user turned
/// reports off (a report by hand goes all the same).
pub fn file(report: Report) -> Option<String> {
    let reporter = REPORTER.get()?;
    let _filing = Filing::enter()?;
    let (code, filed) = file_now(reporter, &report)?;
    if filed {
        let (filed, wake) = &reporter.filed;
        *filed.lock().unwrap_or_else(|e| e.into_inner()) = true;
        wake.notify_one();
    }
    Some(code)
}

/// Queues the failure `bridge::fault!` raised at `file`:`line` and wakes the
/// sender. Called on the thread that raised it.
fn raise(file: &str, line: u32, text: &str) {
    let Some(reporter) = REPORTER.get() else { return };
    let report = Report {
        kind: Kind::Error,
        message: text.to_string(),
        tags: vec![],
        fingerprint: format!("{}:{line}", diagnostics::source_file(file)),
    };
    {
        let mut waiting = FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        if waiting.len() >= MAX_FAULTS {
            return;
        }
        waiting.push_back(report);
    }
    let (filed, wake) = &reporter.filed;
    *filed.lock().unwrap_or_else(|e| e.into_inner()) = true;
    wake.notify_one();
}

/// This thread files a report while it is held; released when dropped, a
/// panic's unwinding included.
struct Filing;

impl Filing {
    fn enter() -> Option<Filing> {
        (!FILING.replace(true)).then_some(Filing)
    }
}

impl Drop for Filing {
    fn drop(&mut self) {
        FILING.set(false);
    }
}

/// The code to quote, and whether a new report was filed for it.
fn file_now(reporter: &Reporter, report: &Report) -> Option<(String, bool)> {
    // The switch is read under the filing lock that `turned_off` takes too:
    // a report either is filed before the outbox is emptied, or sees it off.
    let _filing = reporter.filing.lock().unwrap_or_else(|e| e.into_inner());
    file_in(&reporter.data, report, || context(reporter))
}

/// Files `report` in the outbox of the data folder `data`, with the context
/// `context` makes, as [`file_now`] and [`file_at_exit`] both do: not while
/// reports are off, but for one by hand, and not again today for a failure
/// already sent today by this version.
fn file_in(data: &Path, report: &Report, context: impl FnOnce() -> Context) -> Option<(String, bool)> {
    let by_hand = report.kind == Kind::Manual;
    if !by_hand && !crate::prefs::load(data).send_reports {
        return None;
    }
    let outbox = Outbox::in_data(data);
    let now = SystemTime::now();
    let day = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400;
    let version = env!("CARGO_PKG_VERSION");
    if !by_hand && let Some(code) = outbox.sent_today(version, report.kind, &report.fingerprint, day) {
        return Some((code, false));
    }
    let mut id = [0u8; 16];
    if let Err(e) = getrandom::fill(&mut id) {
        bridge::log!("report: no randomness for its id: {e}");
        return None;
    }
    let event = reports::event(report, &context(), id, now);
    if let Err(e) = outbox.put(&event, now) {
        bridge::log!("report: it cannot be filed: {e}");
        return None;
    }
    if !by_hand {
        outbox.remember(version, report.kind, &report.fingerprint, day, &event.code);
    }
    bridge::log!("report {} filed: {}", event.code, report.kind.tag());
    Some((event.code, true))
}

/// What every report says about the bridge and the computer, and the words
/// it must not carry.
fn context(reporter: &Reporter) -> Context {
    let shared = shared(&reporter.app);
    let mut private: Vec<String> = shared.view().databases.into_iter().map(|d| d.name).collect();
    private.extend(shared.pairing().ok().map(|p| p.code));
    context_with(&reporter.data, shared.strings.lang().code(), reporter.started.elapsed(), private)
}

/// The context of a report from the data folder `data`, with the app's
/// `language`, its `uptime` and the `private` words the app knows.
fn context_with(data: &Path, language: &'static str, uptime: Duration, mut private: Vec<String>) -> Context {
    private.extend(["USERNAME", "COMPUTERNAME"].into_iter().filter_map(|name| std::env::var(name).ok()));
    let channel = if channel().is_store() { "store" } else { "direct" };
    Context {
        version: env!("CARGO_PKG_VERSION").to_string(),
        channel,
        environment: if cfg!(debug_assertions) { reports::DEVELOPMENT } else { channel },
        windows: system::windows_version(),
        webview2: tauri::webview_version().unwrap_or_else(|_| "unknown".to_string()),
        language,
        uptime,
        log: reports::log_tail(&data.join(bridge::log::FILE_NAME)),
        private,
    }
}

/// Files `report` as the app ends because it could not start, and tries once
/// to send what waits (#342): no sender runs after the exit, and an app that
/// keeps failing to start would never send it later. With the reports
/// started, their context; before, one made from the data folder alone.
pub fn file_at_exit(report: Report) {
    let Ok(data) = bridge::start::data_dir() else { return };
    file_at_exit_in(&data, report, &Curl);
}

fn file_at_exit_in(data: &Path, report: Report, transport: &dyn Transport) {
    let Some(dsn) = Dsn::parse(reports::DSN) else { return };
    let context = || match REPORTER.get() {
        Some(reporter) => context(reporter),
        None => {
            let language = crate::prefs::load(data).language(system::display_language()).code();
            context_with(data, language, Duration::ZERO, Vec::new())
        }
    };
    match REPORTER.get() {
        Some(reporter) => {
            let _filing = reporter.filing.lock().unwrap_or_else(|e| e.into_inner());
            file_in(data, &report, context);
        }
        None => {
            file_in(data, &report, context);
        }
    }
    // What waits goes too, the same failure filed by an earlier start that
    // could not send it included: no sender runs to send it later. Asked
    // before each report goes, as the sender asks.
    let automatic = || crate::prefs::load(data).send_reports;
    reports::deliver(&Outbox::in_data(data), &dsn, env!("CARGO_PKG_VERSION"), transport, &automatic);
}

/// Posts what waits, then sleeps until a report is filed, or until the next
/// try of one that got no delivery.
fn send() {
    let Some(reporter) = REPORTER.get() else { return };
    let outbox = Outbox::in_data(&reporter.data);
    let mut failures = 0;
    loop {
        // The failures raised meanwhile, filed here, where no lock is held.
        loop {
            let next = FAULTS.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
            let Some(report) = next else { break };
            let _filing = Filing::enter();
            file_now(reporter, &report);
        }
        // Asked before each report goes: turned off, only reports sent by hand go.
        let automatic = || crate::prefs::load(&reporter.data).send_reports;
        let waits = reports::deliver(&outbox, &reporter.dsn, env!("CARGO_PKG_VERSION"), &Curl, &automatic);
        failures = if waits { failures + 1 } else { 0 };
        let wait = if waits { reports::retry_after(failures) } else { IDLE };
        let (filed, wake) = &reporter.filed;
        let guard = filed.lock().unwrap_or_else(|e| e.into_inner());
        let (mut guard, _) = wake.wait_timeout_while(guard, wait, |filed| !*filed).unwrap_or_else(|e| e.into_inner());
        *guard = false;
    }
}

/// Posts with the system's `curl.exe` (`bridge::tools`).
struct Curl;

impl Transport for Curl {
    fn post(&self, url: &str, headers: &[String], body: &std::path::Path) -> Result<u16, String> {
        bridge::tools::post(url, headers, body)
    }
}

/// Copies a report's `code` to the clipboard, for the user to paste.
pub fn copy(app: &AppHandle, code: &str) {
    if let Err(e) = app.clipboard().write_text(code.to_string()) {
        bridge::log!("report: its code cannot be copied: {e}");
    }
}

/// Sends a report by hand, from the tray menu, on a thread of its own: a
/// native question says what goes, then a native message gives the code,
/// copied, or says that none could be made.
pub fn by_hand(app: &AppHandle) {
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("report-by-hand".into()).spawn(move || {
        let shared = shared(&app);
        let strings = &shared.strings;
        if !alerts::ask(strings.get("failure.title"), strings.get("report.ask")) {
            return;
        }
        let report = Report {
            kind: Kind::Manual,
            message: "a report sent by hand".into(),
            tags: vec![],
            fingerprint: "manual".into(),
        };
        match file(report) {
            Some(code) => {
                copy(&app, &code);
                alerts::inform(strings.get("failure.title"), &strings.fill("report.sent", &[("code", &code)]));
            }
            None => alerts::warn(strings.get("failure.title"), strings.get("report.none")),
        }
    });
    if let Err(e) = spawned {
        bridge::log!("report: no thread for a report by hand: {e}");
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Answers each post with `answer`, keeps what was posted, and runs
    /// `then` after each post.
    struct Fake {
        answer: Result<u16, String>,
        posted: RefCell<Vec<String>>,
        then: Box<dyn Fn()>,
    }

    impl Fake {
        fn answering(answer: Result<u16, String>) -> Fake {
            Fake { answer, posted: RefCell::default(), then: Box::new(|| {}) }
        }
    }

    impl Transport for Fake {
        fn post(&self, _url: &str, _headers: &[String], body: &Path) -> Result<u16, String> {
            self.posted.borrow_mut().push(std::fs::read_to_string(body).unwrap());
            (self.then)();
            self.answer.clone()
        }
    }

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bridge-report-at-exit-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn failure(fingerprint: &str) -> Report {
        Report {
            kind: Kind::Error,
            message: "the app cannot start: no tray".into(),
            tags: vec![],
            fingerprint: fingerprint.into(),
        }
    }

    /// A start that fails files its report and sends it before the exit,
    /// once a day like any failure; one that cannot go waits in the outbox for
    /// a later start; none is filed with the reports turned off (#342).
    #[test]
    fn a_failed_start_is_filed_and_sent_before_the_exit_once_a_day() {
        let dir = folder("sent");
        let fake = Fake::answering(Ok(200));
        file_at_exit_in(&dir, failure("x:1"), &fake);
        file_at_exit_in(&dir, failure("x:1"), &fake);
        let posted = fake.posted.borrow();
        assert_eq!(posted.len(), 1, "the same failure goes once a day");
        assert!(posted[0].contains("the app cannot start: no tray") && posted[0].contains(r#""kind":"error""#));
        assert!(Outbox::in_data(&dir).waiting().is_empty(), "sent, it is gone");

        let dir = folder("waits");
        file_at_exit_in(&dir, failure("x:1"), &Fake::answering(Err("no answer".into())));
        assert_eq!(Outbox::in_data(&dir).waiting().len(), 1, "it waits for the next start");
        // The next start fails the same way the same day, online: the report
        // filed before goes, and no second one is filed.
        let online = Fake::answering(Ok(200));
        file_at_exit_in(&dir, failure("x:1"), &online);
        assert_eq!(online.posted.borrow().len(), 1);
        assert!(Outbox::in_data(&dir).waiting().is_empty());

        let dir = folder("off");
        crate::prefs::save(&dir, &crate::prefs::Prefs { send_reports: false, ..Default::default() }).unwrap();
        let unused = Fake::answering(Ok(200));
        file_at_exit_in(&dir, failure("x:1"), &unused);
        assert!(unused.posted.borrow().is_empty() && Outbox::in_data(&dir).waiting().is_empty());
        for name in ["sent", "waits", "off"] {
            let _ = std::fs::remove_dir_all(folder(name));
        }
    }

    /// Reports turned off while the exit sends what waits stop the rest, as
    /// they stop the sender (#342).
    #[test]
    fn reports_turned_off_during_the_exits_sending_stop_it() {
        let dir = folder("turned-off");
        for n in 1..=2 {
            file_in(&dir, &failure(&format!("waiting:{n}")), || context_with(&dir, "en", Duration::ZERO, vec![]));
        }
        let off = dir.clone();
        let fake = Fake {
            answer: Ok(200),
            posted: RefCell::default(),
            then: Box::new(move || {
                crate::prefs::save(&off, &crate::prefs::Prefs { send_reports: false, ..Default::default() }).unwrap();
            }),
        };
        file_at_exit_in(&dir, failure("x:2"), &fake);
        assert_eq!(fake.posted.borrow().len(), 1, "only the post before reports were turned off");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
