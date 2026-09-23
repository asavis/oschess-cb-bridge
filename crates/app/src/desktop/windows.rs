//! The app's three windows: the status flyout by the tray, the settings
//! window and the first-run window. Each page is a file in `ui/`, told its
//! language by a `lang` parameter.
//!
//! Every window is built by [`spawn_window`], on a worker thread. On Windows,
//! building a WebView on the thread that runs the event loop deadlocks, and a
//! synchronous command, a menu handler and a tray handler all run there
//! (Tauri's `WebviewWindowBuilder::new`, "Known issues"). A test in
//! `crate::window_rules` keeps it so.

use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Rect, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    Window, WindowEvent, Wry,
};

use super::shared;

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

type Builder<'a> = WebviewWindowBuilder<'a, Wry, AppHandle>;

/// A window to build: its label, its page in `ui/` with the page's query, the
/// dictionary key of its title, and the rest of its look.
struct Spec {
    label: &'static str,
    page: &'static str,
    query: String,
    title: &'static str,
    configure: fn(Builder<'_>) -> Builder<'_>,
}

/// Builds the window of `spec` on a worker thread, unless it exists already.
fn spawn_window(app: &AppHandle, spec: Spec) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if app.get_webview_window(spec.label).is_some() {
            return;
        }
        if let Err(e) = build_window(&app, &spec) {
            eprintln!("oschess bridge: the {} window: {e}", spec.label);
        }
    });
}

fn build_window(app: &AppHandle, spec: &Spec) -> tauri::Result<WebviewWindow> {
    let lang = shared(app).strings.lang().code();
    let url = WebviewUrl::App(format!("{}.html?lang={lang}{}", spec.page, spec.query).into());
    let builder = WebviewWindowBuilder::new(app, spec.label, url).title(shared(app).strings.get(spec.title));
    (spec.configure)(builder).build()
}

/// Builds the flyout, hidden until the tray mark is clicked.
pub fn create_flyout(app: &AppHandle) {
    spawn_window(
        app,
        Spec {
            label: FLYOUT,
            page: "flyout",
            query: String::new(),
            title: "window.flyout",
            configure: |b| {
                b.inner_size(FLYOUT_WIDTH, FLYOUT_HEIGHT)
                    .decorations(false)
                    .resizable(false)
                    .skip_taskbar(true)
                    .always_on_top(true)
                    .shadow(true)
                    .visible(false)
            },
        },
    );
}

/// Opens the flyout by the tray mark at `rect`, or closes it when it is open.
/// A click in the moment before the flyout is built does nothing.
pub fn toggle_flyout(app: &AppHandle, rect: Rect) {
    let Some(window) = app.get_webview_window(FLYOUT) else { return };
    if window.is_visible().unwrap_or(false) {
        let _ = window.hide();
        return;
    }
    let shared = shared(app);
    let anchor = {
        let mut flyout = shared.flyout.lock().unwrap_or_else(|e| e.into_inner());
        if flyout.hidden_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
            return;
        }
        let scale = window.scale_factor().unwrap_or(1.0);
        let at = rect.position.to_physical::<f64>(scale);
        let size = rect.size.to_physical::<f64>(scale);
        flyout.anchor = Some((at.x, at.y, size.width, size.height));
        flyout.anchor
    };
    place(&window, anchor);
    let _ = window.show();
    let _ = window.set_focus();
}

/// Sizes the flyout to its page, keeping it by the tray mark.
pub fn fit_flyout(app: &AppHandle, height: f64) {
    let Some(window) = app.get_webview_window(FLYOUT) else { return };
    let height = height.clamp(160.0, 720.0);
    let _ = window.set_size(tauri::LogicalSize::new(FLYOUT_WIDTH, height));
    let anchor = shared(app).flyout.lock().unwrap_or_else(|e| e.into_inner()).anchor;
    place(&window, anchor);
}

pub fn hide_flyout(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(FLYOUT) {
        let _ = window.hide();
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
    let _ = window.set_position(PhysicalPosition::new(fx.round() as i32, fy.round() as i32));
}

/// Opens the settings window at `section` (`databases`, `general` or `code`),
/// or brings it forward there.
pub fn open_settings(app: &AppHandle, section: &str) {
    hide_flyout(app);
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        let _ = window.emit_to(SETTINGS, "section", section);
        return;
    }
    let spec = Spec {
        label: SETTINGS,
        page: "settings",
        query: format!("&section={section}"),
        title: "window.settings",
        configure: |b| b.inner_size(960.0, 680.0).min_inner_size(760.0, 520.0).center(),
    };
    spawn_window(app, spec);
}

pub fn open_first_run(app: &AppHandle) {
    let spec = Spec {
        label: FIRST_RUN,
        page: "first-run",
        query: String::new(),
        title: "window.firstRun",
        configure: |b| b.inner_size(620.0, 420.0).resizable(false).maximizable(false).minimizable(false).center(),
    };
    spawn_window(app, spec);
}

/// The flyout hides when it loses the focus, as the system's own flyouts do.
pub fn on_event(window: &Window, event: &WindowEvent) {
    if window.label() != FLYOUT {
        return;
    }
    if let WindowEvent::Focused(false) = event {
        let _ = window.hide();
        let shared = shared(window.app_handle());
        shared.flyout.lock().unwrap_or_else(|e| e.into_inner()).hidden_at = Some(Instant::now());
    }
}
