//! The commands the windows call. Each window's capability file allows only
//! the ones it needs.

use std::path::Path;
use std::time::Duration;

use bridge::{config, token};
use serde::Serialize;
use tauri::{AppHandle, Manager};
use tauri_plugin_autostart::ManagerExt;
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use super::server::{self, Pairing};
use super::{SharedState, shared, tray, updater, windows};
use crate::prefs;
use crate::settings::{self, Extra};
use crate::status::View;

/// What the settings window shows beside the databases.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    version: &'static str,
    port: u16,
    extras: Vec<Extra>,
    autostart: bool,
    auto_update: bool,
    /// Whether this build looks for updates at all.
    updates: bool,
}

type Answer<T> = Result<T, String>;

fn text<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

#[tauri::command]
pub fn view(shared: SharedState<'_>) -> View {
    shared.view()
}

#[tauri::command]
pub fn open_oschess(app: AppHandle) {
    open_oschess_now(&app);
}

/// Opens the oschess Library's ChessBase section, where the paired browser
/// connects by itself.
pub fn open_oschess_now(app: &AppHandle) {
    windows::hide_flyout(app);
    let _ = app.opener().open_url(shared(app).section_url(), None::<&str>);
}

/// Asynchronous, like every command that may open a window: a synchronous
/// command runs on the event loop's thread.
#[tauri::command]
pub async fn open_settings(app: AppHandle, section: Option<String>) {
    windows::open_settings(&app, section.as_deref().unwrap_or("databases"));
}

#[tauri::command]
pub fn hide_flyout(app: AppHandle) {
    windows::hide_flyout(&app);
}

#[tauri::command]
pub fn fit_flyout(app: AppHandle, height: f64) {
    windows::fit_flyout(&app, height);
}

#[tauri::command]
pub fn settings(app: AppHandle) -> Answer<SettingsView> {
    settings_view(&app)
}

fn settings_view(app: &AppHandle) -> Answer<SettingsView> {
    let shared = shared(app);
    let dir = shared.dir()?;
    let config = config::load_or_create(&shared.config_path()?)?;
    Ok(SettingsView {
        version: env!("CARGO_PKG_VERSION"),
        port: config.port,
        extras: settings::extras(&config),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        auto_update: prefs::load(&dir).auto_update,
        updates: updater::enabled(app),
    })
}

/// Asks for a folder and adds it; `None` when the user cancelled.
#[tauri::command]
pub async fn add_folder(app: AppHandle) -> Answer<Option<SettingsView>> {
    let mut dialog = app.dialog().file().set_title(shared(&app).strings.get("settings.folders.add"));
    if let Some(window) = app.get_webview_window(windows::SETTINGS) {
        dialog = dialog.set_parent(&window);
    }
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_folder()).await.map_err(text)?;
    let Some(folder) = picked else { return Ok(None) };
    let folder = folder.into_path().map_err(text)?;
    change_config(&app, |c| settings::with_database(c, folder.clone()))?;
    settings_view(&app).map(Some)
}

#[tauri::command]
pub fn remove_database(app: AppHandle, path: String) -> Answer<SettingsView> {
    change_config(&app, |c| settings::without_database(c, Path::new(&path)))?;
    settings_view(&app)
}

/// Writes the settings `change` makes to `bridge.toml`.
fn change_config(app: &AppHandle, change: impl FnOnce(&config::Config) -> config::Config) -> Answer<()> {
    let path = shared(app).config_path()?;
    let next = change(&config::load_or_create(&path)?);
    config::save(&path, &next)
}

#[tauri::command]
pub fn set_autostart(app: AppHandle, on: bool) -> Answer<SettingsView> {
    switch_autostart(&app, on)?;
    settings_view(&app)
}

/// Writes or removes the Run key, and moves the menu's tick with it.
pub fn switch_autostart(app: &AppHandle, on: bool) -> Answer<()> {
    let launcher = app.autolaunch();
    let result = if on { launcher.enable() } else { launcher.disable() };
    tray::show_autostart(app, launcher.is_enabled().unwrap_or(false));
    result.map_err(text)
}

#[tauri::command]
pub fn set_auto_update(app: AppHandle, on: bool) -> Answer<SettingsView> {
    let dir = shared(&app).dir()?;
    prefs::save(&dir, &prefs::Prefs { auto_update: on })?;
    settings_view(&app)
}

/// Looks for an update now; the outcome comes as a notification.
#[tauri::command]
pub fn check_updates(app: AppHandle) {
    updater::look_now(&app);
}

#[tauri::command]
pub fn pairing_code(shared: SharedState<'_>) -> Answer<Pairing> {
    shared.pairing()
}

#[tauri::command]
pub fn copy_code(app: AppHandle) -> Answer<()> {
    let code = shared(&app).pairing()?.code;
    app.clipboard().write_text(code).map_err(text)
}

/// Makes a new pairing code and restarts the bridge with it: the running
/// server keeps accepting the old one. The restarted bridge opens the pairing
/// link, which pairs the default browser again.
#[tauri::command]
pub fn new_code(app: AppHandle) -> Answer<()> {
    let dir = shared(&app).dir()?;
    token::replace(&dir).map_err(text)?;
    server::pair_on_next_start(&dir)?;
    restart_soon(app);
    Ok(())
}

/// Saves a new port and restarts the bridge on it, which then opens the
/// pairing link. An invalid port is refused with the dictionary key of the
/// message.
#[tauri::command]
pub fn set_port(app: AppHandle, port: String) -> Answer<()> {
    let port = settings::parse_port(&port).ok_or("settings.port.error")?;
    change_config(&app, |c| config::Config { port, ..c.clone() })?;
    // A paired browser looks for the old port; the pairing link carries the new one.
    server::pair_on_next_start(&shared(&app).dir()?)?;
    restart_soon(app);
    Ok(())
}

/// Restarts the app once the command's answer has reached the window.
fn restart_soon(app: AppHandle) {
    let _ = std::thread::Builder::new().name("restart".into()).spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        app.restart();
    });
}

#[tauri::command]
pub fn open_pairing(app: AppHandle) -> Answer<()> {
    open_pairing_now(&app);
    Ok(())
}

/// Opens oschess with the pairing link, which connects the browser.
pub fn open_pairing_now(app: &AppHandle) {
    if let Ok(pairing) = shared(app).pairing() {
        let _ = app.opener().open_url(pairing.link, None::<&str>);
    }
}
