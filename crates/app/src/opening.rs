//! Opening a window, followed stage by stage (#332). A window is *requested*
//! when its build starts, *built* when Tauri returns it, and *ready* when its
//! page says it has drawn what it shows first. A watcher asks
//! [`Openings::overdue`] once [`LIMIT`] has passed: a window that never got
//! ready is reported with the stage it stopped at. Before, such a click did
//! nothing and left nothing in the log.
//!
//! A window is built only where WebView2 can draw it, as the WebView2 loader
//! answered at the start ([`WebView2AtStart`], [`webview2_found`], #389).

use bridge::sync::lock;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::i18n::Strings;
use crate::reports::{Kind, Report};

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

/// What a click on the tray mark does with the flyout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlyoutClick {
    /// There is none: build the one with this number.
    Build(u64),
    /// This one is open: close it.
    Close(u64),
    /// One is being built: it shows when its page is ready.
    Wait,
}

/// Which flyout is wanted, which is the only one that may live (#365). Each
/// window of the flyout has a number of its own, in its label, and every
/// event says which one it is about: an event about any but the wanted one
/// changes nothing here and only sees that window go. So a late build, a page
/// ready as its window is destroyed or the failure of an opening a later click
/// replaced cannot touch the flyout someone is waiting for.
#[derive(Debug, Default)]
pub struct FlyoutLife {
    /// The numbers given so far.
    made: u64,
    /// The flyout someone wants: being built, or open.
    wanted: Option<u64>,
    /// Whether the wanted one has been shown.
    shown: bool,
}

impl FlyoutLife {
    pub fn shown(&self) -> bool {
        self.shown
    }

    /// The flyout someone wants, if any.
    pub fn current(&self) -> Option<u64> {
        self.wanted
    }

    /// A click on the tray mark.
    pub fn click(&mut self) -> FlyoutClick {
        match self.wanted {
            Some(flyout) if self.shown => {
                self.wanted = None;
                self.shown = false;
                FlyoutClick::Close(flyout)
            }
            Some(_) => FlyoutClick::Wait,
            None => {
                self.made += 1;
                self.wanted = Some(self.made);
                FlyoutClick::Build(self.made)
            }
        }
    }

    /// `flyout`'s page has drawn: whether to show it. Otherwise it goes.
    pub fn ready(&mut self, flyout: u64) -> bool {
        let wanted = self.wanted == Some(flyout);
        self.shown |= wanted;
        wanted
    }

    /// Whether `flyout`, whose window has just been built, is no longer
    /// wanted, and goes.
    pub fn unwanted(&self, flyout: u64) -> bool {
        self.wanted != Some(flyout)
    }

    /// `flyout` failed to open. `None` when it is showing after all, and
    /// stays; otherwise it goes, and whether a click was waiting for it.
    pub fn failed(&mut self, flyout: u64) -> Option<bool> {
        if self.wanted != Some(flyout) {
            return Some(false);
        }
        if self.shown {
            return None;
        }
        self.wanted = None;
        Some(true)
    }

    /// `flyout` lost the focus: whether it closes. WebView2 takes the focus
    /// and gives it back while it is built, so only a shown one does.
    pub fn focus_lost(&mut self, flyout: u64) -> bool {
        let closes = self.shown && self.wanted == Some(flyout);
        if closes {
            self.wanted = None;
            self.shown = false;
        }
        closes
    }

    /// Nobody wants the flyout any more: the one to destroy, if any.
    pub fn abandon(&mut self) -> Option<u64> {
        self.shown = false;
        self.wanted.take()
    }
}

/// The latest opening of each window, by its label.
#[derive(Default)]
pub struct Openings {
    latest: Mutex<HashMap<String, Opening>>,
    /// The last attempt given, of any window: an attempt is never named twice,
    /// so the watcher of one that ended never takes a later one for it (#334).
    attempts: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
struct Opening {
    attempt: u64,
    stage: Stage,
    since: Instant,
}

impl Openings {
    /// Starts an opening of `label`'s window at `now`, in place of any before;
    /// the attempt it returns names this one. `None` while an opening of it is
    /// under way: a second click before the window exists must not build it
    /// twice, since Tauri refuses the second build and that would read as a
    /// failure.
    pub fn request(&self, label: &str, now: Instant) -> Option<u64> {
        let mut latest = self.lock();
        let before = latest.get(label);
        if before.is_some_and(|o| o.stage != Stage::Ready && now.saturating_duration_since(o.since) < LIMIT) {
            return None;
        }
        let attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        latest.insert(label.to_string(), Opening { attempt, stage: Stage::Requested, since: now });
        Some(attempt)
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
        lock(&self.latest)
    }
}

/// A rectangle in physical pixels: left, top, width, height.
pub type Rect = (i32, i32, u32, u32);

/// Whether enough of `window` lies on one of `screens` to be seen and
/// dragged: a strip of its top at least 100 by 40 pixels. Windows answers a
/// window on no screen with the nearest one, so only the rectangles tell.
pub fn on_a_screen(window: Rect, screens: &[Rect]) -> bool {
    let (left, top, width, _) = window;
    let strip = (i64::from(left), i64::from(top), i64::from(left) + i64::from(width), i64::from(top) + 40);
    screens.iter().any(|&(x, y, w, h)| {
        let (x, y) = (i64::from(x), i64::from(y));
        let across = strip.2.min(x + i64::from(w)) - strip.0.max(x);
        let down = strip.3.min(y + i64::from(h)) - strip.1.max(y);
        across >= 100 && down >= 40
    })
}

/// What the WebView2 loader answered when the app started (#389): the WebView2
/// Runtime's version, or why none was found (`tauri::webview_version()`).
/// Tauri asks the same once, as it starts, and keeps the answer: with no
/// runtime then, it refuses every WebView2 until the app ends, and a release
/// build also shows a message of its own, in English, on the event loop,
/// which holds the loop until it is closed. A runtime installed later is used
/// only after a restart, so the bridge keeps the same answer.
pub struct WebView2AtStart(OnceLock<Result<String, String>>);

impl WebView2AtStart {
    pub const fn new() -> WebView2AtStart {
        WebView2AtStart(OnceLock::new())
    }

    /// Keeps the answer `lookup` gives; once one is kept, nothing changes it.
    pub fn remember(&self, lookup: impl FnOnce() -> Result<String, String>) {
        self.0.get_or_init(lookup);
    }

    /// The answer kept at the start, or, before there is one, `now`'s.
    pub fn answer(&self, now: impl FnOnce() -> Result<String, String>) -> Result<String, String> {
        self.0.get().cloned().unwrap_or_else(now)
    }
}

impl Default for WebView2AtStart {
    fn default() -> WebView2AtStart {
        WebView2AtStart::new()
    }
}

/// Whether the window `window` may be built: only where WebView2 can draw it
/// (#389). `lookup` is the loader's answer at the start ([`WebView2AtStart`]).
/// Where it found no runtime, Tauri refuses the WebView2 on the event loop
/// after it has returned the window, and only logs why, so such a window
/// stopped at *built* and never loaded.
///
/// With a runtime, it answers `true` and does nothing else. Without one, it
/// answers `false`, and the caller builds nothing: `file` gets one report of
/// the kind `webview2` about `window`, and answers the code to quote, if any.
/// Then `tell` gets the native message's title and text in `strings`'
/// language, which name Microsoft's download, and that code.
pub fn webview2_found<E: Display>(
    lookup: Result<String, E>,
    window: &str,
    strings: &Strings,
    file: impl FnOnce(Report) -> Option<String>,
    tell: impl FnOnce(&str, &str, Option<&str>),
) -> bool {
    let missing = match lookup {
        Ok(_) => return true,
        Err(e) => e,
    };
    let code = file(Report {
        kind: Kind::WebView2,
        message: format!("no WebView2 Runtime was found, so the {window} window is not built: {missing}"),
        tags: vec![("window", window.to_string())],
        // One failure, whichever window found it first: one report a day.
        fingerprint: "no runtime".to_string(),
    });
    let mut text = strings.get("failure.webview2").to_string();
    if let Some(code) = &code {
        text.push_str("\n\n");
        text.push_str(&strings.fill("failure.code", &[("code", code)]));
    }
    tell(strings.get("failure.title"), &text, code.as_deref());
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click builds a flyout, its page shows it, and a click closes it;
    /// a click while it is being built builds nothing more.
    #[test]
    fn a_click_opens_and_closes_the_flyout() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert_eq!(life.click(), FlyoutClick::Wait, "a second click joins the build");
        assert_eq!(life.current(), Some(1));
        assert!(life.ready(1));
        assert!(life.shown());
        assert_eq!(life.click(), FlyoutClick::Close(1));
        assert_eq!(life.current(), None);
        assert_eq!(life.click(), FlyoutClick::Build(2), "each window has its own number");
    }

    /// WebView2 takes the focus and gives it back while it is built: only a
    /// flyout that was shown closes when it loses the focus.
    #[test]
    fn only_a_shown_flyout_closes_on_losing_the_focus() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert!(!life.focus_lost(1), "being built");
        assert_eq!(life.current(), Some(1));
        assert!(life.ready(1));
        assert!(life.focus_lost(1));
        assert_eq!(life.current(), None);
        assert!(!life.focus_lost(1), "once");
    }

    /// A page that never got ready: the click is told and its window goes.
    /// Its build may end later still, and its page come: that window goes.
    #[test]
    fn a_flyout_whose_page_fails_goes() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert_eq!(life.failed(1), Some(true));
        assert!(life.unwanted(1), "a build that ends after the deadline");
        assert!(!life.ready(1), "a page that comes after the deadline");
        assert_eq!(life.click(), FlyoutClick::Build(2), "the next click starts afresh");
    }

    /// A flyout nobody wants any more goes when it is built, however late.
    #[test]
    fn an_abandoned_build_goes() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert_eq!(life.abandon(), Some(1));
        assert_eq!(life.abandon(), None);
        assert!(life.unwanted(1));
    }

    /// The failure of an opening a later click replaced cancels nothing; nor
    /// does the page of a window that is being destroyed answer for the one
    /// that replaced it.
    #[test]
    fn an_old_flyout_touches_no_newer_one() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert_eq!(life.failed(1), Some(true));
        assert_eq!(life.click(), FlyoutClick::Build(2));
        assert_eq!(life.failed(1), Some(false), "it goes; nobody is told");
        assert!(!life.ready(1), "the old page");
        assert!(!life.shown());
        assert_eq!(life.current(), Some(2), "the new one is still waited for");
        assert!(!life.unwanted(2));
        assert!(life.ready(2));
        assert_eq!(life.click(), FlyoutClick::Close(2));
    }

    /// A failure reported as the page came leaves the flyout that is showing.
    #[test]
    fn a_failure_leaves_a_flyout_that_is_showing() {
        let mut life = FlyoutLife::default();
        assert_eq!(life.click(), FlyoutClick::Build(1));
        assert!(life.ready(1));
        assert_eq!(life.failed(1), None);
        assert_eq!(life.current(), Some(1));
    }

    #[test]
    fn a_window_that_gets_ready_is_not_overdue() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now()).unwrap();
        openings.built("settings", attempt);
        openings.ready("settings");
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    #[test]
    fn an_opening_is_overdue_at_the_stage_it_stopped_at() {
        let openings = Openings::default();
        let never_built = openings.request("settings", Instant::now()).unwrap();
        assert_eq!(openings.overdue("settings", never_built), Some(Stage::Requested));
        let never_ready = openings.request("first-run", Instant::now()).unwrap();
        openings.built("first-run", never_ready);
        assert_eq!(openings.overdue("first-run", never_ready), Some(Stage::Built));
    }

    #[test]
    fn a_page_ready_before_tauri_returned_stays_ready() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now()).unwrap();
        openings.ready("settings");
        openings.built("settings", attempt);
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    #[test]
    fn a_second_click_while_the_window_is_being_built_builds_nothing() {
        let openings = Openings::default();
        let start = Instant::now();
        let first = openings.request("settings", start).unwrap();
        assert_eq!(openings.request("settings", start + LIMIT / 2), None);
        openings.built("settings", first);
        assert_eq!(openings.request("settings", start + LIMIT / 2), None);
        assert_eq!(openings.overdue("settings", first), Some(Stage::Built), "the first stays watched");
        // Once it is overdue, a click may try again.
        assert!(openings.request("settings", start + LIMIT).is_some());
    }

    #[test]
    fn a_later_opening_replaces_the_one_before() {
        let openings = Openings::default();
        let start = Instant::now();
        let first = openings.request("settings", start).unwrap();
        let second = openings.request("settings", start + LIMIT).unwrap();
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
        let attempt = openings.request("settings", Instant::now()).unwrap();
        openings.failed("settings", attempt);
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    /// The watcher of an opening that ended does not take the next one for
    /// its own (#334).
    #[test]
    fn an_attempt_is_never_named_twice() {
        let openings = Openings::default();
        let start = Instant::now();
        let first = openings.request("settings", start).unwrap();
        openings.built("settings", first);
        openings.ready("settings");
        openings.closed("settings");
        let second = openings.request("settings", start + LIMIT * 9 / 10).unwrap();
        assert_ne!(first, second);
        openings.built("settings", second);
        assert_eq!(openings.overdue("settings", first), None, "the first watcher has nothing to report");
        // The same after a failed build.
        openings.failed("settings", second);
        let third = openings.request("settings", start + LIMIT).unwrap();
        assert!(third != first && third != second);
        assert_eq!(openings.overdue("settings", second), None);
    }

    #[test]
    fn a_window_closed_before_its_page_was_ready_is_not_reported() {
        let openings = Openings::default();
        let attempt = openings.request("settings", Instant::now()).unwrap();
        openings.built("settings", attempt);
        openings.closed("settings");
        assert_eq!(openings.overdue("settings", attempt), None);
    }

    /// The window destroyed to be built again may report its end after the
    /// next build was requested: that request stays watched.
    #[test]
    fn closing_the_window_before_keeps_the_next_request() {
        let openings = Openings::default();
        let next = openings.request("settings", Instant::now()).unwrap();
        openings.closed("settings");
        assert_eq!(openings.overdue("settings", next), Some(Stage::Requested));
    }

    #[test]
    fn a_window_on_no_screen_is_told_apart() {
        let screens = [(0, 0, 1920, 1040), (1920, 0, 2560, 1400)];
        assert!(on_a_screen((100, 100, 960, 680), &screens));
        assert!(on_a_screen((1800, 200, 960, 680), &screens), "across two screens");
        assert!(on_a_screen((-860, 10, 960, 680), &screens), "its right end on the screen");
        // The screen it was on was unplugged.
        assert!(!on_a_screen((4600, 100, 960, 680), &screens));
        assert!(!on_a_screen((-32000, -32000, 160, 28), &screens), "where Windows keeps a minimized window");
        // Only its bottom on a screen: the title bar is out of reach.
        assert!(!on_a_screen((100, -660, 960, 680), &screens));
        assert!(!on_a_screen((100, 100, 960, 680), &[]));
    }

    #[test]
    fn a_window_whose_page_never_got_ready_is_not_brought_forward() {
        let openings = Openings::default();
        let start = Instant::now();
        assert!(openings.usable("settings", start), "a window with no opening on record");
        let attempt = openings.request("settings", start).unwrap();
        openings.built("settings", attempt);
        assert!(openings.usable("settings", start + LIMIT / 2), "still loading");
        assert!(!openings.usable("settings", start + LIMIT), "stopped at built");
        openings.ready("settings");
        assert!(openings.usable("settings", start + LIMIT * 10));
    }

    /// What [`webview2_found`] did: whether the window may be built, the
    /// reports it filed, and the messages it showed with their codes.
    struct Opened {
        built: bool,
        filed: Vec<Report>,
        told: Vec<(String, String, Option<String>)>,
    }

    /// Opens `window` as `windows::spawn_window` does, `lookup` the loader's
    /// answer and `code` what filing a report answers.
    fn open(lookup: Result<String, String>, window: &str, strings: &Strings, code: Option<&str>) -> Opened {
        let mut filed = Vec::new();
        let mut told = Vec::new();
        let found = webview2_found(
            lookup,
            window,
            strings,
            |report| {
                filed.push(report);
                code.map(str::to_string)
            },
            |title, text, code| told.push((title.to_string(), text.to_string(), code.map(str::to_string))),
        );
        Opened { built: found, filed, told }
    }

    /// Where the WebView2 loader finds no runtime, no window is built: one
    /// report of its own kind is filed, and the user is told why in the app's
    /// language, with the report's code (#389). A runtime found builds as
    /// before, and says nothing.
    #[test]
    fn without_webview2_no_window_is_built_and_the_user_is_told_why() {
        use crate::i18n::Lang;
        let missing = || Err(NOT_FOUND.to_string());
        let mut texts = Vec::new();
        for lang in [Lang::Uk, Lang::En] {
            let strings = Strings::new(lang);
            let opened = open(missing(), "first-run", &strings, Some("3f9a1c2e"));
            assert!(!opened.built, "no window is built");
            let [report] = opened.filed.as_slice() else { panic!("one report, not {}", opened.filed.len()) };
            assert_eq!((report.kind, report.kind.tag()), (Kind::WebView2, "webview2"));
            assert_eq!(report.tags, [("window", "first-run".to_string())]);
            assert!(
                report.message.contains("first-run") && report.message.contains("0x80070002"),
                "{}",
                report.message
            );
            let [(title, text, code)] = opened.told.as_slice() else { panic!("told once, not {}", opened.told.len()) };
            assert_eq!(title, strings.get("failure.title"));
            let words = strings.get("failure.webview2");
            assert_ne!(words, "failure.webview2", "the dictionary has the text");
            assert!(
                words.contains("developer.microsoft.com/microsoft-edge/webview2"),
                "it names the download: {words}"
            );
            assert_eq!(*text, format!("{words}\n\n{}", strings.fill("failure.code", &[("code", "3f9a1c2e")])));
            assert_eq!(code.as_deref(), Some("3f9a1c2e"));
            texts.push(words.to_string());

            // Another window that finds no runtime is the same failure, so the
            // outbox sends one report of it a day.
            let flyout = open(missing(), "flyout", &strings, Some("3f9a1c2e"));
            assert_eq!(flyout.filed[0].fingerprint, report.fingerprint);
            // With the reports turned off, the user is told all the same, with
            // no code.
            let off = open(missing(), "settings", &strings, None);
            assert!(!off.built);
            assert_eq!(off.told, [(title.clone(), words.to_string(), None)]);
        }
        assert_ne!(texts[0], texts[1], "each language has its own text");
        assert!(texts[0].starts_with("Міст") && texts[1].starts_with("The bridge"), "{texts:?}");

        let found = open(Ok("142.0.3595.53".into()), "first-run", &Strings::new(Lang::En), Some("3f9a1c2e"));
        assert!(found.built && found.filed.is_empty() && found.told.is_empty());
    }

    /// What the loader answers where no runtime is installed, as
    /// `windows::core::Error` prints it.
    const NOT_FOUND: &str = "The system cannot find the file specified. (0x80070002)";

    /// A runtime installed while the bridge runs is used only after a restart,
    /// as Tauri keeps what it found as it started (#389). So the answer kept at
    /// the start stays, no window is built, and the message says to start the
    /// bridge again.
    #[test]
    fn a_runtime_installed_after_the_start_waits_for_a_restart() {
        use crate::i18n::Lang;
        let installed = || Ok("142.0.3595.53".to_string());
        let at_start = WebView2AtStart::new();
        assert_eq!(at_start.answer(installed), installed(), "before the start, a lookup now");
        at_start.remember(|| Err(NOT_FOUND.to_string()));
        // The runtime is installed: a later lookup finds it.
        at_start.remember(installed);
        let answer = at_start.answer(installed);
        assert_eq!(answer, Err(NOT_FOUND.to_string()), "the start's answer stays");
        for (lang, quit) in
            [(Lang::Uk, "(меню біля годинника → «Вийти»)"), (Lang::En, "(the menu by the clock → “Quit”)")]
        {
            let strings = Strings::new(lang);
            let opened = open(answer.clone(), "settings", &strings, None);
            assert!(!opened.built, "no window is built");
            let [(_, text, _)] = opened.told.as_slice() else { panic!("told once, not {}", opened.told.len()) };
            assert!(text.contains(quit) && text.contains(strings.get("menu.quit")), "it says to restart: {text}");
        }
    }
}
