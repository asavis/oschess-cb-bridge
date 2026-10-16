//! Sending the failure reports (#333, `crate::reports`). [`file`] puts a
//! report in the outbox and wakes the sender, a thread of its own that posts
//! it with the system's `curl.exe`. Neither the event loop nor WebView2 takes
//! part, so a report leaves when they are what failed.

use std::cell::Cell;
use std::path::PathBuf;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tauri::AppHandle;
use tauri_plugin_clipboard_manager::ClipboardExt;

use super::{alerts, channel, shared, system};
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
    if let Err(e) = std::thread::Builder::new().name("reports".into()).spawn(send) {
        bridge::log!("report: no sender: {e}");
    }
}

/// Whether this build sends reports at all.
pub fn available() -> bool {
    Dsn::parse(reports::DSN).is_some()
}

/// Files `report` and wakes the sender. Answers the code a user can quote,
/// or `None` when nothing goes: this build has no DSN, the user turned
/// reports off (a report by hand goes all the same), or the same failure
/// went today.
pub fn file(report: Report) -> Option<String> {
    let reporter = REPORTER.get()?;
    if FILING.get() {
        return None;
    }
    FILING.set(true);
    let code = file_now(reporter, &report);
    FILING.set(false);
    let code = code?;
    let (filed, wake) = &reporter.filed;
    *filed.lock().unwrap_or_else(|e| e.into_inner()) = true;
    wake.notify_one();
    Some(code)
}

fn file_now(reporter: &Reporter, report: &Report) -> Option<String> {
    let by_hand = report.kind == Kind::Manual;
    if !by_hand && !crate::prefs::load(&reporter.data).send_reports {
        return None;
    }
    let _filing = reporter.filing.lock().unwrap_or_else(|e| e.into_inner());
    let outbox = Outbox::in_data(&reporter.data);
    let now = SystemTime::now();
    let day = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400;
    if !by_hand && !outbox.first_today(env!("CARGO_PKG_VERSION"), report.kind, &report.fingerprint, day) {
        return None;
    }
    let mut id = [0u8; 16];
    if let Err(e) = getrandom::fill(&mut id) {
        bridge::log!("report: no randomness for its id: {e}");
        return None;
    }
    let event = reports::event(report, &context(reporter), id, now);
    if let Err(e) = outbox.put(&event, now) {
        bridge::log!("report: it cannot be filed: {e}");
        return None;
    }
    bridge::log!("report {} filed: {}", event.code, report.kind.tag());
    Some(event.code)
}

/// What every report says about the bridge and the computer, and the words
/// it must not carry.
fn context(reporter: &Reporter) -> Context {
    let shared = shared(&reporter.app);
    let mut private: Vec<String> = shared.view().databases.into_iter().map(|d| d.name).collect();
    private.extend(shared.pairing().ok().map(|p| p.code));
    private.extend(["USERNAME", "COMPUTERNAME"].into_iter().filter_map(|name| std::env::var(name).ok()));
    let channel = if channel().is_store() { "store" } else { "direct" };
    Context {
        version: env!("CARGO_PKG_VERSION").to_string(),
        channel,
        environment: if cfg!(debug_assertions) { reports::DEVELOPMENT } else { channel },
        windows: system::windows_version(),
        webview2: tauri::webview_version().unwrap_or_else(|_| "unknown".to_string()),
        language: shared.strings.lang().code(),
        uptime: reporter.started.elapsed(),
        log: reports::log_tail(&reporter.data.join(bridge::log::FILE_NAME)),
        private,
    }
}

/// Posts what waits, then sleeps until a report is filed, or until the next
/// try of one that got no delivery.
fn send() {
    let Some(reporter) = REPORTER.get() else { return };
    let outbox = Outbox::in_data(&reporter.data);
    let mut failures = 0;
    loop {
        let waits = reports::deliver(&outbox, &reporter.dsn, env!("CARGO_PKG_VERSION"), &Curl);
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
