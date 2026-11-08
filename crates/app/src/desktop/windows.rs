//! The app's three windows: the status flyout by the tray, the settings
//! window and the first-run wizard. Each page is a file in `ui/`, told its
//! language by a `lang` parameter.
//!
//! Every window is built by [`spawn_window`], on a worker thread. On Windows,
//! building a WebView on the thread that runs the event loop deadlocks, and a
//! synchronous command, a menu handler and a tray handler all run there
//! (Tauri's `WebviewWindowBuilder::new`, "Known issues"). A test in
//! `crate::window_rules` keeps it so.
//!
//! Opening a window is followed stage by stage (#332, `crate::opening`): a
//! window whose page is not ready within `opening::LIMIT` is logged with the
//! stage it stopped at, and the user is told so in a native message. Before,
//! the click did nothing and left nothing in the log.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Rect, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    Window, WindowEvent, Wry,
};

use super::{alerts, faults, reporting, shared};
use crate::i18n::Strings;
use crate::opening::{self, FlyoutClick, LIMIT, Stage};
use crate::reports::{Kind, Report};
use crate::status::Asked;

/// Translate native titles as well as the documents inside their webviews.
pub fn translate(app: &AppHandle) {
    let shared = shared(app);
    for window in app.webview_windows().values() {
        translate_window(window, &shared.strings);
    }
}

pub fn translate_window(window: &WebviewWindow, strings: &Strings) {
    let title = match window.label() {
        SETTINGS => strings.get("window.settings"),
        FLYOUT => strings.get("window.flyout"),
        FIRST_RUN => strings.get("window.firstRun"),
        _ => return,
    };
    if let Err(e) = window.set_title(title) {
        bridge::fault!("the {} window's title: {e}", window.label());
    }
}

pub const FLYOUT: &str = "flyout";
pub const SETTINGS: &str = "settings";
pub const FIRST_RUN: &str = "first-run";

/// The flyout's width, and its height until the page measures itself.
const FLYOUT_WIDTH: f64 = 380.0;
const FLYOUT_HEIGHT: f64 = 480.0;
/// The gap between the flyout and the taskbar or the screen's edge.
const MARGIN: f64 = 12.0;
/// A click on the tray mark takes the focus from the open flyout first; a
/// click this soon after that must not open it again.
const REOPEN_GUARD: Duration = Duration::from_millis(300);
/// How long a window destroyed to be built again may take to be gone.
const GONE_WAIT: Duration = Duration::from_secs(5);

type Builder<'a> = WebviewWindowBuilder<'a, Wry, AppHandle>;

/// A window to build: its label, its page in `ui/` with the page's query, its
/// title in the app's language, and the rest of its look.
struct Spec {
    label: &'static str,
    page: &'static str,
    query: String,
    title: String,
    configure: fn(Builder<'_>) -> Builder<'_>,
}

/// Builds the window of `spec` on a worker thread, unless it exists already,
/// and watches it get ready.
fn spawn_window(app: &AppHandle, spec: Spec) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if app.get_webview_window(spec.label).is_some() {
            return;
        }
        // A second click while the window is being built builds nothing.
        let Some(attempt) = shared(&app).openings.request(spec.label, Instant::now()) else { return };
        let opening = shared(&app).flyout.lock().unwrap_or_else(|e| e.into_inner()).life.opening();
        watch_opening(&app, spec.label, attempt, opening);
        if faults::on("build-hangs") {
            std::future::pending::<()>().await;
        }
        match build_window(&app, &spec) {
            Ok(_) => {
                shared(&app).openings.built(spec.label, attempt);
                // A click may have given up on it while it was built, and its
                // page may never come.
                if spec.label == FLYOUT {
                    settle_flyout(&app);
                }
            }
            Err(e) => {
                let line = format!("the {} window: {e}", spec.label);
                bridge::log!("{line}");
                shared(&app).openings.failed(spec.label, attempt);
                failed_to_open(&app, spec.label, opening, Stage::Requested, &line);
            }
        }
    });
}

fn build_window(app: &AppHandle, spec: &Spec) -> Result<WebviewWindow, String> {
    if faults::on("build-fails") {
        return Err("the fault build-fails".to_string());
    }
    let lang = shared(app).strings.lang().code();
    let url = WebviewUrl::App(format!("{}.html?lang={lang}{}", spec.page, spec.query).into());
    let builder = WebviewWindowBuilder::new(app, spec.label, url).title(spec.title.as_str());
    (spec.configure)(builder).build().map_err(|e| e.to_string())
}

/// Watches `attempt` of `label`'s window from a thread of its own: a page that
/// is not ready after [`LIMIT`] is logged, and the user is told.
fn watch_opening(app: &AppHandle, label: &'static str, attempt: u64, opening: u64) {
    let watched = app.clone();
    let spawned = std::thread::Builder::new().name("window-watch".into()).spawn(move || {
        std::thread::sleep(LIMIT);
        let Some(stage) = shared(&watched).openings.overdue(label, attempt) else { return };
        let line = format!("the {label} window stopped at \"{}\" after {} s", stage.name(), LIMIT.as_secs());
        bridge::log!("{line}");
        failed_to_open(&watched, label, opening, stage, &line);
    });
    if let Err(e) = spawned {
        bridge::fault!("the {label} window is not watched: {e}");
    }
}

/// Reports that `label`'s window did not open (#333), its log `line` the
/// report's message, and tells the user in a native message how far it got,
/// with the report's code, copied. A flyout no click waits for any more is
/// reported and not shown: nobody is waiting for it.
fn failed_to_open(app: &AppHandle, label: &'static str, opening: u64, stage: Stage, line: &str) {
    let code = reporting::file(Report {
        kind: Kind::Window,
        message: line.to_string(),
        tags: vec![("window", label.to_string()), ("stage", stage.name().to_string())],
        fingerprint: format!("{label} {}", stage.name()),
    });
    let shared = shared(app);
    let strings = &shared.strings;
    // The click is answered here, and the window its page failed in goes.
    let waited = label == FLYOUT && flyout_failed(app, opening);
    let what = match label {
        SETTINGS => strings.get("failure.window.settings"),
        FIRST_RUN => strings.get("failure.window.firstRun"),
        FLYOUT if waited => strings.get("failure.window.flyout"),
        _ => return,
    };
    let why = match stage {
        Stage::Requested => strings.get("failure.stage.requested"),
        Stage::Built => strings.get("failure.stage.built"),
        Stage::Ready => return,
    };
    let mut text = format!("{what}\n\n{why}\n\n{}", strings.get("failure.advice"));
    if let Some(code) = code {
        reporting::copy(app, &code);
        text.push_str("\n\n");
        text.push_str(&strings.fill("failure.code", &[("code", &code)]));
    }
    alerts::warn(strings.get("failure.title"), &text);
}

/// The flyout's window, hidden when built: it shows once its page is ready.
fn flyout_spec(app: &AppHandle) -> Spec {
    Spec {
        label: FLYOUT,
        page: "flyout",
        query: String::new(),
        title: shared(app).strings.get("window.flyout").to_string(),
        configure: |b| {
            b.inner_size(FLYOUT_WIDTH, FLYOUT_HEIGHT)
                .decorations(false)
                .resizable(false)
                .skip_taskbar(true)
                .always_on_top(true)
                .shadow(true)
                .visible(false)
        },
    }
}

/// Builds the flyout, hidden.
fn create_flyout(app: &AppHandle) {
    spawn_window(app, flyout_spec(app));
}

/// Opens the flyout by the tray mark at `rect`, or closes it when it is open.
/// The flyout exists only while someone uses it, as its WebView2 processes
/// hold some 200 MB (#365): a click builds it, and it shows when its page is
/// ready ([`page_ready`]).
pub fn toggle_flyout(app: &AppHandle, rect: Rect) {
    let window = app.get_webview_window(FLYOUT);
    let visible = window.as_ref().is_some_and(|w| w.is_visible().unwrap_or(false));
    let shared = shared(app);
    let click = shared.openings.flyout_click(FLYOUT, window.is_some(), visible, Instant::now());
    if click == FlyoutClick::Close {
        hide_flyout(app);
        return;
    }
    let closing = {
        let mut flyout = shared.flyout.lock().unwrap_or_else(|e| e.into_inner());
        if flyout.hidden_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
            return;
        }
        let scale = window
            .as_ref()
            .and_then(|w| w.scale_factor().ok())
            .or_else(|| app.primary_monitor().ok().flatten().map(|m| m.scale_factor()))
            .unwrap_or(1.0);
        let at = rect.position.to_physical::<f64>(scale);
        let size = rect.size.to_physical::<f64>(scale);
        flyout.anchor = Some((at.x, at.y, size.width, size.height));
        // Numbered when it starts a build, so that an older one's failure
        // cancels nothing.
        if matches!(click, FlyoutClick::Build | FlyoutClick::Rebuild) || flyout.life.closing() {
            flyout.life.begin();
        } else {
            flyout.life.wait();
        }
        flyout.life.closing()
    };
    shared.ask(Asked::Window);
    if closing && window.is_some() {
        build_when_gone(app, flyout_spec(app));
        return;
    }
    match (click, window) {
        (FlyoutClick::Show, Some(window)) => show_flyout(app, &window),
        (FlyoutClick::Rebuild, Some(window)) => match window.destroy() {
            Ok(()) => build_when_gone(app, flyout_spec(app)),
            Err(e) => bridge::fault!("the flyout cannot be destroyed: {e}"),
        },
        (FlyoutClick::Build, _) => create_flyout(app),
        _ => {}
    }
}

/// Puts the flyout by the tray mark and shows it.
fn show_flyout(app: &AppHandle, window: &WebviewWindow) {
    let anchor = {
        let shared = shared(app);
        let mut flyout = shared.flyout.lock().unwrap_or_else(|e| e.into_inner());
        flyout.life.give_up();
        flyout.anchor
    };
    place(window, anchor);
    if let Err(e) = window.show().and_then(|()| window.set_focus()) {
        bridge::fault!("the flyout cannot be shown: {e}");
    }
}

/// A page has drawn: the flyout a click waits for shows now, and one nobody
/// waits for any more goes.
pub fn page_ready(app: &AppHandle, window: &WebviewWindow) {
    if window.label() != FLYOUT {
        return;
    }
    if shared(app).flyout.lock().unwrap_or_else(|e| e.into_inner()).life.waiting() {
        show_flyout(app, window);
    } else {
        settle_flyout(app);
    }
}

/// Destroys a flyout nobody wants any more, as decided on the event loop's
/// thread: a click or a page handled there is seen, and one handled later
/// finds it going.
fn settle_flyout(app: &AppHandle) {
    let handle = app.clone();
    if let Err(e) = app.run_on_main_thread(move || settle_now(&handle)) {
        bridge::fault!("the flyout is not settled: {e}");
    }
}

/// [`settle_flyout`], on the event loop's thread.
fn settle_now(app: &AppHandle) {
    let window = app.get_webview_window(FLYOUT);
    let visible = window.as_ref().is_some_and(|w| w.is_visible().unwrap_or(false));
    let shared = shared(app);
    let unwanted = shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.settle(window.is_some(), visible);
    if let Some(window) = window.filter(|_| unwanted)
        && let Err(e) = window.destroy()
    {
        shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.closed();
        bridge::fault!("the flyout cannot be destroyed: {e}");
    }
}

/// The flyout's opening numbered `opening` failed: on the event loop's thread,
/// where the clicks are handled, the click waiting for it gives up and the
/// window goes, unless a newer click replaced it. Whether a click waited, to
/// be told. Call it off that thread: it waits for the answer.
fn flyout_failed(app: &AppHandle, opening: u64) -> bool {
    let (answer, answered) = mpsc::channel();
    let handle = app.clone();
    let queued = app.run_on_main_thread(move || {
        let (latest, waited) = {
            let shared = shared(&handle);
            let mut flyout = shared.flyout.lock().unwrap_or_else(|e| e.into_inner());
            (flyout.life.opening() == opening, flyout.life.fail(opening))
        };
        if latest {
            settle_now(&handle);
        }
        // The caller gave up waiting only after a long stall, and then knows.
        answer.send(waited).ok();
    });
    if let Err(e) = queued {
        bridge::fault!("the flyout's failure is not handled: {e}");
        return false;
    }
    answered.recv_timeout(LIMIT).unwrap_or(false)
}

/// Sizes the flyout to its page, keeping it by the tray mark.
pub fn fit_flyout(app: &AppHandle, height: f64) {
    let Some(window) = app.get_webview_window(FLYOUT) else { return };
    let height = height.clamp(160.0, 720.0);
    if let Err(e) = window.set_size(tauri::LogicalSize::new(FLYOUT_WIDTH, height)) {
        bridge::fault!("the flyout cannot be sized: {e}");
    }
    let anchor = shared(app).flyout.lock().unwrap_or_else(|e| e.into_inner()).anchor;
    place(&window, anchor);
}

/// Closes the flyout: its window goes, and its WebView2 processes with it
/// (#365).
pub fn hide_flyout(app: &AppHandle) {
    let shared = shared(app);
    let Some(window) = app.get_webview_window(FLYOUT) else {
        shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.give_up();
        return;
    };
    if !shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.close() {
        return;
    }
    if let Err(e) = window.destroy() {
        shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.closed();
        bridge::fault!("the flyout cannot be closed: {e}");
    }
}

/// Puts the flyout above the tray mark, or below it when the taskbar is at
/// the top, inside the screen's work area.
fn place(window: &WebviewWindow, anchor: Option<(f64, f64, f64, f64)>) {
    let Some((x, y, w, h)) = anchor else { return };
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let Ok(Some(monitor)) = window.monitor_from_point(cx, cy) else { return };
    let scale = monitor.scale_factor();
    let work = monitor.work_area();
    let (left, top) = (f64::from(work.position.x), f64::from(work.position.y));
    let (width, height) = (f64::from(work.size.width), f64::from(work.size.height));
    let size = window.outer_size().unwrap_or(PhysicalSize::new(0, 0));
    let (fw, fh) = (f64::from(size.width), f64::from(size.height));
    let margin = MARGIN * scale;
    let fx = (cx - fw / 2.0).clamp(left + margin, (left + width - fw - margin).max(left + margin));
    let fy = if cy > top + height / 2.0 { top + height - fh - margin } else { top + margin };
    if let Err(e) = window.set_position(PhysicalPosition::new(fx.round() as i32, fy.round() as i32)) {
        bridge::fault!("the flyout cannot be placed: {e}");
    }
}

/// Opens the settings window at `section` (`databases`, `general` or `code`),
/// or brings it forward there. A settings window that cannot be brought
/// forward, or whose page never got ready, is destroyed and built again:
/// before, every later click found it again and did nothing again (#332).
pub fn open_settings(app: &AppHandle, section: &str) {
    hide_flyout(app);
    let spec = Spec {
        label: SETTINGS,
        page: "settings",
        query: format!("&section={section}"),
        title: shared(app).strings.get("window.settings").to_string(),
        configure: |b| b.inner_size(960.0, 680.0).min_inner_size(760.0, 520.0).center(),
    };
    // Asked by the windows a user opens only: the flyout, built hidden at the
    // start, must not make the bridge look at every database then (#365).
    shared(app).ask(Asked::Window);
    let Some(window) = app.get_webview_window(SETTINGS) else {
        spawn_window(app, spec);
        return;
    };
    let shown = if shared(app).openings.usable(SETTINGS, Instant::now()) {
        bring_forward(&window)
    } else {
        Err("its page never got ready".to_string())
    };
    match shown {
        Ok(()) => {
            if let Err(e) = window.emit_to(SETTINGS, "section", section) {
                bridge::fault!("the settings window's section: {e}");
            }
        }
        Err(e) => {
            bridge::fault!("the settings window cannot be brought forward, so it is built again: {e}");
            match window.destroy() {
                Ok(()) => build_when_gone(app, spec),
                Err(e) => bridge::fault!("the settings window cannot be destroyed: {e}"),
            }
        }
    }
}

/// Shows `window` on a screen, restored and focused.
fn bring_forward(window: &WebviewWindow) -> Result<(), String> {
    if faults::on("show-fails") {
        return Err("the fault show-fails".to_string());
    }
    let text = |e: tauri::Error| e.to_string();
    window.unminimize().map_err(text)?;
    window.show().map_err(text)?;
    // On no screen, as after the screen it was on went away: to the main one.
    if !on_a_screen(window)? {
        bridge::log!("the settings window was on no screen and is moved to the main one");
        to_primary(window)?;
    }
    window.set_focus().map_err(text)
}

/// Whether `window` can be seen on one of the screens (`opening::on_a_screen`).
fn on_a_screen(window: &WebviewWindow) -> Result<bool, String> {
    let text = |e: tauri::Error| e.to_string();
    let (at, size) = (window.outer_position().map_err(text)?, window.outer_size().map_err(text)?);
    let screens: Vec<opening::Rect> = window
        .available_monitors()
        .map_err(text)?
        .iter()
        .map(|m| (m.position().x, m.position().y, m.size().width, m.size().height))
        .collect();
    Ok(opening::on_a_screen((at.x, at.y, size.width, size.height), &screens))
}

/// Centres `window` in the work area of the primary screen.
fn to_primary(window: &WebviewWindow) -> Result<(), String> {
    let text = |e: tauri::Error| e.to_string();
    let monitor = window.primary_monitor().map_err(text)?.ok_or_else(|| "no screen".to_string())?;
    let work = monitor.work_area();
    let size = window.outer_size().map_err(text)?;
    let centre = |start: i32, room: u32, used: u32| start.saturating_add_unsigned(room.saturating_sub(used) / 2);
    let x = centre(work.position.x, work.size.width, size.width);
    let y = centre(work.position.y, work.size.height, size.height);
    window.set_position(PhysicalPosition::new(x, y)).map_err(text)
}

/// Builds `spec`'s window once the one destroyed before it is gone: the event
/// loop removes a destroyed window a moment later, and [`spawn_window`] builds
/// none while a window of the label is still there.
fn build_when_gone(app: &AppHandle, spec: Spec) {
    let (app, label) = (app.clone(), spec.label);
    let spawned = std::thread::Builder::new().name("window-again".into()).spawn(move || {
        let start = Instant::now();
        while app.get_webview_window(spec.label).is_some() {
            if start.elapsed() > GONE_WAIT {
                bridge::fault!(
                    "the {} window is still there {} s after it was destroyed",
                    spec.label,
                    GONE_WAIT.as_secs()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        spawn_window(&app, spec);
    });
    if let Err(e) = spawned {
        bridge::fault!("the {label} window is not built again: {e}");
    }
}

/// Opens the first-run wizard (#201): its steps, then the pairing screen.
pub fn open_first_run(app: &AppHandle) {
    let spec = Spec {
        label: FIRST_RUN,
        page: "first-run",
        query: String::new(),
        title: shared(app).strings.get("window.firstRun").to_string(),
        configure: |b| b.inner_size(760.0, 720.0).resizable(false).maximizable(false).minimizable(false).center(),
    };
    shared(app).ask(Asked::Window);
    spawn_window(app, spec);
}

/// The flyout closes when it loses the focus, as the system's own flyouts do.
/// A window that is gone ends its opening (#332): one closed before its page
/// was ready is not reported as stopped.
pub fn on_event(window: &Window, event: &WindowEvent) {
    match event {
        WindowEvent::Destroyed => {
            let shared = shared(window.app_handle());
            shared.openings.closed(window.label());
            if window.label() == FLYOUT {
                shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.closed();
            }
        }
        WindowEvent::Focused(false) if window.label() == FLYOUT => {
            let shared = shared(window.app_handle());
            // WebView2 takes the focus and gives it back while it is built:
            // only a flyout that was shown closes when it loses the focus.
            if shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).life.waiting() {
                return;
            }
            hide_flyout(window.app_handle());
            shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).hidden_at = Some(Instant::now());
        }
        _ => {}
    }
}
