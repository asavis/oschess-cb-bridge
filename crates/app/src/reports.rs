//! Failure reports (#333): the failures #332 records, sent to oschess's
//! GlitchTip in the envelope format of Sentry, which GlitchTip speaks. They
//! are sent by default, until the user turns them off in the settings; a
//! report the user sends from the tray menu goes even then.
//!
//! What a report holds is a fixed list ([`event`]): the kind of failure and
//! where it happened, the bridge's version and channel, the versions of
//! Windows and WebView2, the app's language, how long it had run, and the end
//! of `bridge.log`, which names no file, database, game or query of the
//! user's (#117). Every text passes through [`clean`] before it leaves:
//! paths become `<path>`, and the words of [`Context::private`], such as the
//! pairing code and the databases' names, `<private>`. A report has no user,
//! no machine name and no address.
//!
//! A report waits in the [`Outbox`], a folder in the data folder, until
//! [`deliver`] has it answered: on `2xx` it is deleted, on `429`, `5xx` or no
//! answer it waits for a later try, and on any other answer it is dropped.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::diagnostics;

/// Where reports go: the `bridge` project of oschess's GlitchTip
/// (asavis/oschess#16588). Its key only lets a client submit events, as the
/// updater's public key only checks them. Empty, nothing is sent.
pub const DSN: &str = "";

/// The `environment` of a debug build's reports, apart from the users'.
pub const DEVELOPMENT: &str = "development";

/// How many of the log's last lines a report carries.
pub const LOG_LINES: usize = 100;

/// The most reports the outbox keeps, and the most bytes; the oldest go first.
pub const MAX_FILES: usize = 20;
pub const MAX_BYTES: u64 = 512 << 10;

/// A report's text is cut to this many characters.
const TEXT_CHARS: usize = 500;

/// A Sentry DSN: `https://<key>@<host>/<project>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dsn {
    key: String,
    host: String,
    project: String,
}

impl Dsn {
    /// The DSN `dsn`; `None` for anything but an HTTPS DSN with a hex key and
    /// a numbered project, the empty one of a build without one included.
    pub fn parse(dsn: &str) -> Option<Dsn> {
        let (key, rest) = dsn.strip_prefix("https://")?.split_once('@')?;
        let (host, project) = rest.trim_end_matches('/').split_once('/')?;
        let key_ok = !key.is_empty() && key.chars().all(|c| c.is_ascii_hexdigit());
        let host_ok = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
        let project_ok = !project.is_empty() && project.chars().all(|c| c.is_ascii_digit());
        (key_ok && host_ok && project_ok).then(|| Dsn { key: key.into(), host: host.into(), project: project.into() })
    }

    /// The project's envelope endpoint.
    pub fn url(&self) -> String {
        format!("https://{}/api/{}/envelope/", self.host, self.project)
    }

    /// The headers of a post from the bridge of `version`.
    pub fn headers(&self, version: &str) -> Vec<String> {
        vec![
            "Content-Type: application/x-sentry-envelope".to_string(),
            format!(
                "X-Sentry-Auth: Sentry sentry_version=7, sentry_key={}, sentry_client=oschess-bridge/{version}",
                self.key
            ),
            format!("User-Agent: oschess-bridge/{version}"),
        ]
    }
}

/// The kind of a failure, as the tag `kind` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A window of the bridge's own did not open (#332).
    Window,
    Panic,
    /// The event loop stopped answering (#332).
    Stall,
    /// An error in a window's script (#332).
    Script,
    /// A report the user sent from the tray menu.
    Manual,
}

impl Kind {
    pub fn tag(self) -> &'static str {
        match self {
            Kind::Window => "window",
            Kind::Panic => "panic",
            Kind::Stall => "stall",
            Kind::Script => "script",
            Kind::Manual => "manual",
        }
    }

    fn level(self) -> &'static str {
        if self == Kind::Manual { "info" } else { "error" }
    }
}

/// A failure to report.
#[derive(Clone, Debug)]
pub struct Report {
    pub kind: Kind,
    /// The failure's line, as the log has it.
    pub message: String,
    /// Where it happened, beside the context's tags: `window`, `stage` and
    /// `busy`.
    pub tags: Vec<(&'static str, String)>,
    /// What makes two reports one failure: sent once a day per version.
    pub fingerprint: String,
}

/// What every report says about the bridge and the computer.
#[derive(Clone, Debug, Default)]
pub struct Context {
    pub version: String,
    /// `store` or `direct`.
    pub channel: &'static str,
    /// [`DEVELOPMENT`] in a debug build, else the channel.
    pub environment: &'static str,
    /// Windows' version and build, such as `10.0.26100.4652 24H2`.
    pub windows: String,
    pub webview2: String,
    /// The app's language, `uk` or `en`.
    pub language: &'static str,
    pub uptime: Duration,
    /// The end of `bridge.log`, oldest line first.
    pub log: Vec<String>,
    /// Words no report may carry: the pairing code, the databases' names,
    /// the user's and the computer's names.
    pub private: Vec<String>,
}

/// A report ready to go.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The event's id, 32 hex digits.
    pub id: String,
    /// The report code a user quotes: the id's first 8 digits, also the tag
    /// `report`.
    pub code: String,
    /// The envelope to post.
    pub envelope: String,
}

/// `text` without paths and without the words of `private`, shortened.
pub fn clean(text: &str, private: &[String]) -> String {
    let mut text = diagnostics::without_paths(text);
    // The longest first: a name that holds another goes whole.
    let mut words: Vec<&String> = private.iter().filter(|w| w.chars().count() >= 3).collect();
    words.sort_by_key(|w| std::cmp::Reverse(w.len()));
    for word in words {
        text = text.replace(word.as_str(), "<private>");
    }
    text.chars().take(TEXT_CHARS).collect()
}

/// The report `report` in `context`, as the event `id` at `now`.
pub fn event(report: &Report, context: &Context, id: [u8; 16], now: SystemTime) -> Event {
    let id: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let code = id[..8].to_string();
    let clean = |text: &str| clean(text, &context.private);
    let mut tags = BTreeMap::new();
    tags.insert("kind", report.kind.tag().to_string());
    tags.insert("report", code.clone());
    tags.insert("channel", context.channel.to_string());
    tags.insert("windows", clean(&context.windows));
    tags.insert("webview2", clean(&context.webview2));
    tags.insert("language", context.language.to_string());
    for (name, value) in &report.tags {
        tags.insert(name, clean(value));
    }
    let from = context.log.len().saturating_sub(LOG_LINES);
    let log: Vec<String> = context.log[from..].iter().map(|l| clean(l)).collect();
    let payload = json!({
        "event_id": id,
        "timestamp": now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs(),
        "platform": "native",
        "level": report.kind.level(),
        "logger": "oschess-bridge",
        // The version alone: oschess's issue reader keeps a release's first
        // 12 characters, a web build's short SHA (asavis/oschess#16588).
        "release": context.version,
        "environment": context.environment,
        "message": { "formatted": clean(&report.message) },
        "tags": tags,
        "contexts": {
            "os": { "name": "Windows", "version": clean(&context.windows) },
            "app": { "app_version": context.version },
        },
        "extra": { "log": log, "uptimeSeconds": context.uptime.as_secs() },
        "fingerprint": [report.kind.tag(), clean(&report.fingerprint)],
    })
    .to_string();
    let header = json!({ "event_id": id });
    let item = json!({ "type": "event", "length": payload.len() });
    Event { id, code, envelope: format!("{header}\n{item}\n{payload}\n") }
}

/// The reports waiting to go: `reports` in the data folder, one file each,
/// named by the time they were filed so that the oldest sorts first.
pub struct Outbox {
    dir: PathBuf,
}

/// The extension of a waiting report.
const ENVELOPE: &str = "envelope";

impl Outbox {
    pub fn in_data(data: &Path) -> Outbox {
        Outbox { dir: data.join("reports") }
    }

    /// Files `event` at `now`; the outbox then drops its oldest reports
    /// beyond [`MAX_FILES`] and [`MAX_BYTES`].
    pub fn put(&self, event: &Event, now: SystemTime) -> io::Result<PathBuf> {
        std::fs::create_dir_all(&self.dir)?;
        let millis = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis();
        let path = self.dir.join(format!("{millis:015}-{}.{ENVELOPE}", event.id));
        // Written aside and moved in, so the sender never reads half a report.
        let part = path.with_extension("part");
        std::fs::write(&part, &event.envelope)?;
        std::fs::rename(&part, &path)?;
        self.trim();
        Ok(path)
    }

    /// The waiting reports, oldest first.
    pub fn waiting(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut files: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == ENVELOPE)).collect();
        files.sort();
        files
    }

    fn trim(&self) {
        let mut files: Vec<(PathBuf, u64)> = self
            .waiting()
            .into_iter()
            .map(|p| {
                let size = std::fs::metadata(&p).map_or(0, |m| m.len());
                (p, size)
            })
            .collect();
        let mut total: u64 = files.iter().map(|(_, size)| size).sum();
        while files.len() > MAX_FILES || (total > MAX_BYTES && files.len() > 1) {
            let (oldest, size) = files.remove(0);
            if std::fs::remove_file(&oldest).is_ok() {
                bridge::log!("report: the outbox is full, so its oldest report is dropped");
            }
            total = total.saturating_sub(size);
        }
    }

    /// The file that records which failures went today.
    fn seen(&self) -> PathBuf {
        self.dir.join("seen.json")
    }

    /// Whether the failure `fingerprint` of `kind` in `version` is the first
    /// of its kind on `day` (days since 1970), and records it: one failure
    /// goes at most once a day per version, so that one that repeats does
    /// not flood the project. Records older than yesterday are forgotten.
    pub fn first_today(&self, version: &str, kind: Kind, fingerprint: &str, day: u64) -> bool {
        let mut digest = bridge::sha256::Sha256::new();
        digest.update(format!("{version}\n{}\n{fingerprint}", kind.tag()).as_bytes());
        let key = bridge::sha256::hex(&digest.finish())[..16].to_string();
        let path = self.seen();
        let mut seen: BTreeMap<String, u64> =
            std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default();
        seen.retain(|_, d| *d + 1 >= day);
        if seen.get(&key) == Some(&day) {
            return false;
        }
        seen.insert(key, day);
        let written = std::fs::create_dir_all(&self.dir)
            .and_then(|()| std::fs::write(&path, serde_json::to_vec(&seen).unwrap_or_default()));
        if let Err(e) = written {
            bridge::log!("report: {} cannot be written: {e}", "seen.json");
        }
        true
    }
}

/// How a post is made: the system's `curl.exe` on Windows, a stand-in in
/// tests.
pub trait Transport {
    /// Posts `body` to `url` and answers the HTTP status; `Err` when no
    /// answer came.
    fn post(&self, url: &str, headers: &[String], body: &Path) -> Result<u16, String>;
}

/// What becomes of a report after a post.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Sent,
    /// No answer, `429` or `5xx`: it waits for a later try.
    Again,
    /// Any other answer: sending it again would get the same.
    Dropped,
}

pub fn outcome(answer: &Result<u16, String>) -> Outcome {
    match answer {
        Ok(200..=299) => Outcome::Sent,
        Ok(429 | 500..) | Err(_) => Outcome::Again,
        Ok(_) => Outcome::Dropped,
    }
}

/// How long the sender waits after `failures` tries in a row got no
/// delivery: a minute, doubled each time, at most an hour.
pub fn retry_after(failures: u32) -> Duration {
    let minutes = 1u64 << failures.saturating_sub(1).min(6);
    Duration::from_secs(minutes * 60).min(Duration::from_secs(3600))
}

/// Posts every waiting report, oldest first, until one must wait. Answers
/// whether one still waits.
pub fn deliver(outbox: &Outbox, dsn: &Dsn, version: &str, transport: &dyn Transport) -> bool {
    let (url, headers) = (dsn.url(), dsn.headers(version));
    for file in outbox.waiting() {
        let answer = transport.post(&url, &headers, &file);
        match outcome(&answer) {
            Outcome::Sent => forget(&file),
            Outcome::Dropped => {
                bridge::log!("report: refused with {}, so it is dropped", answer.unwrap_or(0));
                forget(&file);
            }
            Outcome::Again => {
                if let Err(e) = answer {
                    bridge::log!("report: {e}; it waits for a later try");
                }
                return true;
            }
        }
    }
    false
}

/// The most of the log's end [`log_tail`] reads.
const LOG_TAIL_BYTES: u64 = 64 << 10;

/// The last lines of the log at `path`, oldest first: from at most its last
/// 64 KiB, without the line that cut runs through. None when it cannot be
/// read.
pub fn log_tail(path: &Path) -> Vec<String> {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else { return Vec::new() };
    let from = file.metadata().map_or(0, |m| m.len()).saturating_sub(LOG_TAIL_BYTES);
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(from)).and_then(|_| file.take(LOG_TAIL_BYTES).read_to_end(&mut bytes)).is_err() {
        return Vec::new();
    }
    let mut lines: Vec<String> = String::from_utf8_lossy(&bytes).lines().map(String::from).collect();
    if from > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    lines
}

/// Removes a report that went or was dropped; one that stays would go again.
fn forget(file: &Path) {
    if let Err(e) = std::fs::remove_file(file) {
        bridge::log!("report: a report that went cannot be removed: {e}");
    }
}

/// The event's JSON in `envelope`, the third of its lines.
pub fn payload(envelope: &str) -> Option<Value> {
    serde_json::from_str(envelope.lines().nth(2)?).ok()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const KEY: &str = "0123456789abcdef0123456789abcdef";

    fn dsn() -> Dsn {
        Dsn::parse(&format!("https://{KEY}@glitchtip.example.org/4")).unwrap()
    }

    fn context() -> Context {
        Context {
            version: "1.2610.1601".into(),
            channel: "store",
            environment: "store",
            windows: "10.0.26100.4652 24H2".into(),
            webview2: "131.0.2903.70".into(),
            language: "uk",
            uptime: Duration::from_secs(3600),
            log: (0..150).map(|n| format!("2026-10-16T10:00:00Z line {n}")).collect(),
            private: vec!["Mega Database 2024".into(), "Olena".into(), "tokentokentokentoken".into()],
        }
    }

    fn window_report() -> Report {
        Report {
            kind: Kind::Window,
            message: "the settings window stopped at \"built\" after 10 s".into(),
            tags: vec![("window", "settings".into()), ("stage", "built".into())],
            fingerprint: "settings built".into(),
        }
    }

    #[test]
    fn a_dsn_is_read_strictly() {
        let dsn = dsn();
        assert_eq!(dsn.url(), "https://glitchtip.example.org/api/4/envelope/");
        let auth = &dsn.headers("1.2")[1];
        assert_eq!(
            auth,
            &format!("X-Sentry-Auth: Sentry sentry_version=7, sentry_key={KEY}, sentry_client=oschess-bridge/1.2")
        );
        for bad in [
            "",
            "http://abc@host/1",
            "https://@host/1",
            "https://xyz!@host/1",
            "https://abc@host/",
            "https://abc@host/a",
            "https://abc@ho st/1",
        ] {
            assert_eq!(Dsn::parse(bad), None, "{bad:?}");
        }
        assert_eq!(Dsn::parse(DSN).is_some(), !DSN.is_empty(), "the built-in DSN, when there is one, is valid");
    }

    #[test]
    fn an_event_holds_the_fixed_fields() {
        let event = event(&window_report(), &context(), [0xab; 16], UNIX_EPOCH + Duration::from_secs(1_792_141_000));
        assert_eq!(event.id, "ab".repeat(16));
        assert_eq!(event.code, "abababab");
        let lines: Vec<&str> = event.envelope.lines().collect();
        assert_eq!(lines.len(), 3);
        let header: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(header["event_id"], event.id);
        let item: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(item["type"], "event");
        assert_eq!(item["length"], lines[2].len());
        let p = payload(&event.envelope).unwrap();
        assert_eq!(p["level"], "error");
        assert_eq!(p["release"], "1.2610.1601");
        assert_eq!(p["environment"], "store");
        assert_eq!(p["timestamp"], 1_792_141_000);
        assert_eq!(p["tags"]["kind"], "window");
        assert_eq!(p["tags"]["report"], "abababab");
        assert_eq!(p["tags"]["window"], "settings");
        assert_eq!(p["tags"]["stage"], "built");
        assert_eq!(p["tags"]["webview2"], "131.0.2903.70");
        assert_eq!(p["fingerprint"], json!(["window", "settings built"]));
        let log = p["extra"]["log"].as_array().unwrap();
        assert_eq!(log.len(), LOG_LINES, "the end of the log only");
        assert_eq!(log[LOG_LINES - 1], "2026-10-16T10:00:00Z line 149");
        // Nothing names the user or the machine.
        let fields: Vec<&String> = p.as_object().unwrap().keys().collect();
        for absent in ["user", "server_name", "request", "breadcrumbs", "modules"] {
            assert!(!fields.iter().any(|f| *f == absent), "{absent} is not sent");
        }
    }

    /// The allow-list holds: a path, a database's name, the pairing code and
    /// the user's name, put in the log and in the message, do not leave.
    #[test]
    fn nothing_private_leaves() {
        let mut context = context();
        context.log.push(
            r"2026-10-16T10:00:00Z cannot open C:\Users\Olena\Documents\ChessBase\Mega Database 2024.cbh: denied"
                .into(),
        );
        context.log.push("2026-10-16T10:00:00Z served Mega Database 2024 with tokentokentokentoken".into());
        let report = Report {
            kind: Kind::Panic,
            message: "panic in the thread main at x.rs:1: Olena's C:/Users/Olena/x.toml".into(),
            tags: vec![],
            fingerprint: "x.rs:1".into(),
        };
        let event = event(&report, &context, [1; 16], SystemTime::now());
        for private in ["Olena", "Mega Database", "tokentokentokentoken", r"C:\Users", "C:/Users", "ChessBase"] {
            assert!(!event.envelope.contains(private), "{private} left in the report");
        }
        let p = payload(&event.envelope).unwrap();
        assert_eq!(p["message"]["formatted"], "panic in the thread main at x.rs:1: <private>'s <path>");
        let log = p["extra"]["log"].as_array().unwrap();
        assert_eq!(log[LOG_LINES - 2], "2026-10-16T10:00:00Z cannot open <path>: denied");
        assert_eq!(log[LOG_LINES - 1], "2026-10-16T10:00:00Z served <private> with <private>");
    }

    #[test]
    fn a_manual_report_is_not_an_error() {
        let report = Report {
            kind: Kind::Manual,
            message: "a report by hand".into(),
            tags: vec![],
            fingerprint: "manual".into(),
        };
        let p = payload(&event(&report, &context(), [2; 16], SystemTime::now()).envelope).unwrap();
        assert_eq!(p["level"], "info");
        assert_eq!(p["tags"]["kind"], "manual");
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("oschess-bridge-reports-{name}-{}", std::process::id()));
        let _ignored = std::fs::remove_dir_all(&dir);
        dir
    }

    fn filed(outbox: &Outbox, n: u8, at: u64) -> PathBuf {
        let event = event(&window_report(), &context(), [n; 16], UNIX_EPOCH + Duration::from_secs(at));
        outbox.put(&event, UNIX_EPOCH + Duration::from_secs(at)).unwrap()
    }

    #[test]
    fn the_outbox_keeps_the_newest_reports_within_its_caps() {
        let dir = scratch("caps");
        let outbox = Outbox::in_data(&dir);
        let first = filed(&outbox, 1, 1000);
        for n in 2..=(MAX_FILES as u8 + 3) {
            filed(&outbox, n, 1000 + u64::from(n));
        }
        let waiting = outbox.waiting();
        assert_eq!(waiting.len(), MAX_FILES);
        assert!(!waiting.contains(&first), "the oldest went first");
        assert!(waiting.windows(2).all(|w| w[0] < w[1]), "oldest first");
        // By size: a report as big as the cap leaves only itself.
        let big = Event { id: "f".repeat(32), code: "ffffffff".into(), envelope: "x".repeat(MAX_BYTES as usize) };
        outbox.put(&big, UNIX_EPOCH + Duration::from_secs(5000)).unwrap();
        assert_eq!(outbox.waiting().len(), 1);
        assert!(
            std::fs::read_dir(dir.join("reports"))
                .unwrap()
                .flatten()
                .all(|e| e.path().extension() != Some("part".as_ref()))
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failure_goes_once_a_day_per_version() {
        let dir = scratch("seen");
        let outbox = Outbox::in_data(&dir);
        assert!(outbox.first_today("1.0", Kind::Window, "settings built", 20_000));
        assert!(!outbox.first_today("1.0", Kind::Window, "settings built", 20_000));
        assert!(outbox.first_today("1.0", Kind::Window, "settings requested", 20_000), "another failure");
        assert!(outbox.first_today("1.1", Kind::Window, "settings built", 20_000), "another version");
        assert!(outbox.first_today("1.0", Kind::Window, "settings built", 20_001), "the next day");
        // The record stays small: days before yesterday are forgotten.
        assert!(outbox.first_today("1.0", Kind::Panic, "x.rs:1", 20_010));
        let seen: BTreeMap<String, u64> = serde_json::from_slice(&std::fs::read(outbox.seen()).unwrap()).unwrap();
        assert_eq!(seen.len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_log_tail_drops_the_cut_line() {
        let dir = scratch("tail");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("bridge.log");
        assert!(log_tail(&log).is_empty(), "no log yet");
        std::fs::write(&log, "one\ntwo\n").unwrap();
        assert_eq!(log_tail(&log), ["one", "two"]);
        let line = "x".repeat(99);
        let big: String = (0..1000).map(|n| format!("{n:04} {line}\n")).collect();
        std::fs::write(&log, &big).unwrap();
        let tail = log_tail(&log);
        assert_eq!(tail.last().unwrap(), &format!("0999 {line}"));
        assert!(tail.iter().all(|l| l.len() == 104), "no line cut in half");
        assert!(tail.len() < 1000);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn answers_decide_what_becomes_of_a_report() {
        assert_eq!(outcome(&Ok(200)), Outcome::Sent);
        assert_eq!(outcome(&Ok(202)), Outcome::Sent);
        assert_eq!(outcome(&Ok(429)), Outcome::Again);
        assert_eq!(outcome(&Ok(503)), Outcome::Again);
        assert_eq!(outcome(&Err("no answer".into())), Outcome::Again);
        assert_eq!(outcome(&Ok(400)), Outcome::Dropped);
        assert_eq!(outcome(&Ok(403)), Outcome::Dropped);
        assert_eq!(outcome(&Ok(301)), Outcome::Dropped);
        assert_eq!([1, 2, 3, 7, 50].map(retry_after), [60, 120, 240, 3600, 3600].map(Duration::from_secs));
    }

    /// Answers posts from a list, and keeps what it was given.
    struct Fake {
        answers: RefCell<Vec<Result<u16, String>>>,
        posted: RefCell<Vec<(String, Vec<String>, String)>>,
    }

    impl Transport for Fake {
        fn post(&self, url: &str, headers: &[String], body: &Path) -> Result<u16, String> {
            self.posted.borrow_mut().push((url.into(), headers.to_vec(), std::fs::read_to_string(body).unwrap()));
            self.answers.borrow_mut().remove(0)
        }
    }

    #[test]
    fn delivery_stops_at_the_first_report_that_must_wait() {
        let dir = scratch("deliver");
        let outbox = Outbox::in_data(&dir);
        let files: Vec<PathBuf> = (1..=4).map(|n| filed(&outbox, n, 1000 + u64::from(n))).collect();
        let first = std::fs::read_to_string(&files[0]).unwrap();
        let fake =
            Fake { answers: RefCell::new(vec![Ok(200), Ok(400), Err("no answer".into())]), posted: RefCell::default() };
        assert!(deliver(&outbox, &dsn(), "1.2", &fake), "one still waits");
        assert_eq!(outbox.waiting(), files[2..], "sent and refused ones are gone, the rest wait in order");
        let posted = fake.posted.borrow();
        assert_eq!(posted.len(), 3);
        assert_eq!(posted[0].0, "https://glitchtip.example.org/api/4/envelope/");
        assert!(posted[0].1.iter().any(|h| h.starts_with("X-Sentry-Auth: Sentry sentry_version=7")));
        assert_eq!(posted[0].2, first, "the file is posted as it is");
        drop(posted);
        let fake = Fake { answers: RefCell::new(vec![Ok(200), Ok(200)]), posted: RefCell::default() };
        assert!(!deliver(&outbox, &dsn(), "1.2", &fake));
        assert!(outbox.waiting().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
