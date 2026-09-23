//! The app's three windows: the status flyout by the tray, the settings
//! window and the first-run window. Each page is a file in `ui/`, told its
//! language by a `lang` parameter.

use std::time::{Duration, Instant};

use tauri::{
    AppHandle, Emitter, Manager, PhysicalPosition, PhysicalSize, Rect, WebviewUrl, WebviewWindow, WebviewWindowBuilder,
    Window, WindowEvent,
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

fn page(app: &AppHandle, name: &str, query: &str) -> WebviewUrl {
    let lang = shared(app).strings.lang().code();
    WebviewUrl::App(format!("{name}.html?lang={lang}{query}").into())
}

pub fn create_flyout(app: &AppHandle) -> tauri::Result<WebviewWindow> {
    WebviewWindowBuilder::new(app, FLYOUT, page(app, "flyout", ""))
        .title(shared(app).strings.get("window.flyout"))
        .inner_size(FLYOUT_WIDTH, FLYOUT_HEIGHT)
        .decorations(false)
        .resizable(false)
        .skip_taskbar(true)
        .always_on_top(true)
        .shadow(true)
        .visible(false)
        .build()
}

/// Opens the flyout by the tray mark at `rect`, or closes it when it is open.
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
pub fn open_settings(app: &AppHandle, section: &str) -> tauri::Result<()> {
    hide_flyout(app);
    if let Some(window) = app.get_webview_window(SETTINGS) {
        let _ = window.unminimize();
        window.show()?;
        window.set_focus()?;
        return window.emit_to(SETTINGS, "section", section);
    }
    WebviewWindowBuilder::new(app, SETTINGS, page(app, "settings", &format!("&section={section}")))
        .title(shared(app).strings.get("window.settings"))
        .inner_size(960.0, 680.0)
        .min_inner_size(760.0, 520.0)
        .center()
        .build()?;
    Ok(())
}

pub fn open_first_run(app: &AppHandle) -> tauri::Result<()> {
    if let Some(window) = app.get_webview_window(FIRST_RUN) {
        return window.set_focus();
    }
    WebviewWindowBuilder::new(app, FIRST_RUN, page(app, "first-run", ""))
        .title(shared(app).strings.get("window.firstRun"))
        .inner_size(620.0, 420.0)
        .resizable(false)
        .maximizable(false)
        .minimizable(false)
        .center()
        .build()?;
    Ok(())
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
