//! The commands the windows call. Each window's capability file allows only
//! the ones it needs.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use bridge::engines::{self, Roots};
use bridge::stockfish::{self, Build, Progress};
use bridge::{config, engine, token};
use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};
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

/// What the engine section shows: the engines found and the one chosen, the
/// official build the bridge can install, and whether to offer it instead.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnginesView {
    /// The engine `bridge.toml` names, if any.
    chosen: Option<String>,
    found: Vec<FoundEngine>,
    install: Installable,
    /// The chosen engine's name when it is an older Stockfish and the offer
    /// was not put off for this bridge version.
    offer_for: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Installable {
    version: &'static str,
    megabytes: u64,
}

/// An installation's progress, for the settings window.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgress {
    phase: &'static str,
    done: u64,
    total: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FoundEngine {
    name: String,
    path: String,
    source: &'static str,
    /// For a build the bridge installed: its version, whose licence the
    /// window can open.
    version: Option<String>,
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

/// The engines on this computer and the chosen one. Reading the engine
/// folders touches the disk, so it runs off the event loop.
#[tauri::command]
pub async fn engines(app: AppHandle) -> Answer<EnginesView> {
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

fn engines_view(app: &AppHandle) -> Answer<EnginesView> {
    let shared = shared(app);
    let config = config::load_or_create(&shared.config_path()?)?;
    let data = shared.dir()?;
    let roots = Roots { bridge_data: Some(data.clone()), ..Roots::system() };
    let found: Vec<FoundEngine> = engines::find(&roots)
        .into_iter()
        .map(|f| {
            let version = (f.source == engines::BRIDGE)
                .then(|| f.path.parent()?.file_name()?.to_str()?.strip_prefix("stockfish-").map(str::to_string))
                .flatten();
            FoundEngine { name: f.name, path: f.path.to_string_lossy().into_owned(), source: f.source, version }
        })
        .collect();
    let build = Build::for_arch(stockfish::machine_arch());
    let chosen = config.engine.map(|p| p.to_string_lossy().into_owned());
    // The chosen engine's name as found, else its file's name.
    let chosen_name = chosen.as_ref().map(|path| {
        found
            .iter()
            .find(|f| &f.path == path)
            .map(|f| f.name.clone())
            .unwrap_or_else(|| path.rsplit(['\\', '/']).next().unwrap_or(path).trim_end_matches(".exe").to_string())
    });
    // An installed pinned build is in the list already: nothing to offer.
    let dismissed = stockfish::is_installed(&data, build)
        || prefs::load(&data).stockfish_offer_dismissed.as_deref() == Some(env!("CARGO_PKG_VERSION"));
    let offer_for = match (&chosen, &chosen_name) {
        (Some(path), Some(name)) if !dismissed && stockfish::offer(Some((Path::new(path), name))).is_some() => {
            Some(name.clone())
        }
        _ => None,
    };
    Ok(EnginesView {
        chosen,
        found,
        install: Installable { version: build.version, megabytes: build.megabytes() },
        offer_for,
    })
}

/// One installation at a time.
static INSTALLING: Mutex<()> = Mutex::new(());

/// Installs the official Stockfish pinned in this release, then chooses it.
/// The progress goes to the settings window as `stockfish-progress` events.
/// A failure answers why, in English, for the window to show. The download
/// holds no lock another choice waits on: only the probe and the save do.
#[tauri::command]
pub async fn install_stockfish(app: AppHandle) -> Answer<EnginesView> {
    let data = shared(&app).dir()?;
    let config_path = shared(&app).config_path()?;
    let window = app.clone();
    tauri::async_runtime::spawn_blocking(move || -> Answer<()> {
        let Ok(_installing) = INSTALLING.try_lock() else {
            return Err("Stockfish is being installed already".into());
        };
        let build = Build::for_arch(stockfish::machine_arch());
        let exe = stockfish::install(&data, build, &stockfish::System, &mut |progress| {
            let (phase, done, total) = match progress {
                Progress::Downloading { done, total } => ("downloading", done, total),
                Progress::Checking => ("checking", 0, 0),
                Progress::Unpacking => ("unpacking", 0, 0),
            };
            let _ = window.emit_to(windows::SETTINGS, "stockfish-progress", InstallProgress { phase, done, total });
        })?;
        let _one = CHOOSING.lock().unwrap_or_else(PoisonError::into_inner);
        engine::probe(&exe)?;
        let next = config::Config { engine: Some(exe), ..config::load_or_create(&config_path)? };
        config::save(&config_path, &next)
    })
    .await
    .map_err(text)??;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// Opens the licence of the Stockfish `version` the bridge installed. Only a
/// version is taken from the window; the path is the bridge's own.
#[tauri::command]
pub fn open_stockfish_licence(app: AppHandle, version: String) -> Answer<()> {
    if version.is_empty() || !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        return Err("not a Stockfish version".into());
    }
    let licence = shared(&app).dir()?.join("engines").join(format!("stockfish-{version}")).join(stockfish::LICENCE);
    if !licence.is_file() {
        return Err(format!("{} is missing", licence.display()));
    }
    app.opener().open_path(licence.to_string_lossy(), None::<&str>).map_err(text)
}

/// Puts off the Stockfish offer until the next bridge version.
#[tauri::command]
pub async fn dismiss_stockfish_offer(app: AppHandle) -> Answer<EnginesView> {
    let dir = shared(&app).dir()?;
    let prefs = prefs::Prefs { stockfish_offer_dismissed: Some(env!("CARGO_PKG_VERSION").into()), ..prefs::load(&dir) };
    prefs::save(&dir, &prefs)?;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// One choice at a time: a slow probe cannot save its engine over a later one.
static CHOOSING: Mutex<()> = Mutex::new(());

/// Chooses the engine at `path` once it answers as a UCI engine. A file that
/// does not is refused with the dictionary key of the message. The bridge
/// follows `bridge.toml`, so the running engine stops and the new one serves
/// the next analysis.
#[tauri::command]
pub async fn choose_engine(app: AppHandle, path: String) -> Answer<EnginesView> {
    let program = PathBuf::from(path);
    let config_path = shared(&app).config_path()?;
    tauri::async_runtime::spawn_blocking(move || -> Answer<()> {
        let _one = CHOOSING.lock().unwrap_or_else(PoisonError::into_inner);
        engine::probe(&program).map_err(|_| "settings.engine.refused".to_string())?;
        let next = config::Config { engine: Some(program), ..config::load_or_create(&config_path)? };
        config::save(&config_path, &next)
    })
    .await
    .map_err(text)??;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// Asks for an engine's executable and chooses it; `None` when the user cancelled.
#[tauri::command]
pub async fn pick_engine(app: AppHandle) -> Answer<Option<EnginesView>> {
    let strings = &shared(&app).strings;
    let mut dialog = app
        .dialog()
        .file()
        .set_title(strings.get("settings.engine.pick"))
        .add_filter(strings.get("settings.engine.filter"), &["exe"]);
    if let Some(window) = app.get_webview_window(windows::SETTINGS) {
        dialog = dialog.set_parent(&window);
    }
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_file()).await.map_err(text)?;
    let Some(file) = picked else { return Ok(None) };
    let path = file.into_path().map_err(text)?;
    choose_engine(app, path.to_string_lossy().into_owned()).await.map(Some)
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
    prefs::save(&dir, &prefs::Prefs { auto_update: on, ..prefs::load(&dir) })?;
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
