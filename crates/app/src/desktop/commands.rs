//! The commands the windows call. Each window's capability file allows only
//! the ones it needs.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bridge::engines::{self, Roots};
use bridge::stockfish::{self, Build};
use bridge::{config, engine, token};
use serde::Serialize;
use tauri::{AppHandle, Emitter, WebviewWindow};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

use super::autostart::{self, State};
use super::server::{self, Pairing};
use super::{SharedState, channel, shared, tray, updater, windows};
use crate::choices::{self, Choices, EnginesView, InstallProgress};
use crate::prefs;
use crate::settings::{self, Extra, Failure};
use crate::status::View;

/// What the settings window shows beside the databases.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SettingsView {
    version: &'static str,
    port: u16,
    extras: Vec<Extra>,
    autostart: bool,
    /// Windows' own settings turned starting with Windows off, and only they
    /// turn it on again.
    autostart_blocked: bool,
    auto_update: bool,
    /// Whether this build looks for updates at all.
    updates: bool,
    /// Whether the Microsoft Store installed this copy and updates it (#112).
    store: bool,
}

/// A command's answer; a failure goes to the window as a dictionary key and
/// its values ([`Failure`]).
type Answer<T> = Result<T, Failure>;

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

/// Opens the oschess Library's ChessBase section with the pairing link, as
/// the first-run window does (#76): a browser the bridge has not paired yet
/// (another browser, another profile, or after «New code») pairs with nothing
/// to copy, and a paired one reconnects as before. The page reads the link's
/// fragment before anything renders and never sends it to a server.
pub fn open_oschess_now(app: &AppHandle) {
    windows::hide_flyout(app);
    let url = shared(app).pairing().map(|p| p.link).unwrap_or_else(|_| shared(app).section_url());
    let _ = app.opener().open_url(url, None::<&str>);
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
    let autostart = autostart::state(app);
    Ok(SettingsView {
        version: env!("CARGO_PKG_VERSION"),
        port: config.port,
        extras: settings::extras(&config),
        autostart: autostart == State::On,
        autostart_blocked: autostart == State::Blocked,
        auto_update: prefs::load(&dir).auto_update,
        updates: updater::enabled(app),
        store: channel().is_store(),
    })
}

/// Asks for a folder, over the window that asked, and adds it; `None` when
/// the user cancelled.
#[tauri::command]
pub async fn add_folder(app: AppHandle, window: WebviewWindow) -> Answer<Option<SettingsView>> {
    let dialog = app.dialog().file().set_title(shared(&app).strings.get("settings.folders.add")).set_parent(&window);
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

/// Writes the settings `change` makes to `bridge.toml`, under the lock every
/// change of it takes (`config::update`).
fn change_config(app: &AppHandle, change: impl FnOnce(&config::Config) -> config::Config) -> Result<(), String> {
    config::update(&shared(app).config_path()?, change).map(drop)
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
    let found = engines::find(&roots);
    Ok(EnginesView::new(&data, config.engine.as_deref(), found, env!("CARGO_PKG_VERSION")))
}

/// The engine choices and Stockfish installations, in the order
/// [`Choices`] keeps.
static CHOICES: Choices = Choices::new();

/// Whether Stockfish is being installed now (#61): an update waits for it.
pub(super) fn installing() -> bool {
    CHOICES.installing()
}

/// Installs the official Stockfish pinned in this release, then chooses it
/// unless another engine was chosen meanwhile ([`Choices::install`]). The
/// progress goes to the window that asked, the settings or the first-run
/// wizard, as `stockfish-progress` events. A failed installation answers why,
/// in English, inside the message that Stockfish was not installed.
#[tauri::command]
pub async fn install_stockfish(app: AppHandle, window: WebviewWindow) -> Answer<EnginesView> {
    let failed = |message: String| Failure::with("settings.engine.installFailed", message);
    let data = shared(&app).dir().map_err(failed)?;
    let config_path = shared(&app).config_path().map_err(failed)?;
    let label = window.label().to_string();
    tauri::async_runtime::spawn_blocking(move || -> Result<(), String> {
        let install = || {
            let build = Build::for_arch(stockfish::machine_arch());
            stockfish::install(&data, build, &stockfish::System, &mut |progress| {
                let _ = window.emit_to(label.as_str(), "stockfish-progress", InstallProgress::from(progress));
            })
        };
        CHOICES.install(&config_path, install, |exe| engine::probe(exe).map(drop)).map(drop)
    })
    .await
    .map_err(text)
    .and_then(|installed| installed)
    .map_err(failed)?;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// Opens the licence of the Stockfish `version` the bridge installed. Only a
/// version is taken from the window; the path is the bridge's own.
#[tauri::command]
pub fn open_stockfish_licence(app: AppHandle, version: String) -> Answer<()> {
    let licence =
        stockfish::licence(&shared(&app).dir()?, &version).ok_or_else(|| "not a Stockfish version".to_string())?;
    if !licence.is_file() {
        return Err(format!("{} is missing", licence.display()).into());
    }
    Ok(app.opener().open_path(licence.to_string_lossy(), None::<&str>).map_err(text)?)
}

/// Puts off the Stockfish offer until the next bridge version.
#[tauri::command]
pub async fn dismiss_stockfish_offer(app: AppHandle) -> Answer<EnginesView> {
    choices::dismiss_offer(&shared(&app).dir()?, env!("CARGO_PKG_VERSION"))?;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// Chooses the engine at `path` once it answers as a UCI engine. A file that
/// does not is refused with its own message. The bridge follows
/// `bridge.toml`, so the running engine stops and the new one serves the next
/// analysis.
#[tauri::command]
pub async fn choose_engine(app: AppHandle, path: String) -> Answer<EnginesView> {
    let program = PathBuf::from(path);
    let config_path = shared(&app).config_path()?;
    tauri::async_runtime::spawn_blocking(move || -> Answer<()> {
        let probe = |p: &Path| engine::probe(p).map(drop).map_err(|_| Failure::new("settings.engine.refused"));
        CHOICES.choose(&config_path, program, probe)
    })
    .await
    .map_err(text)??;
    tauri::async_runtime::spawn_blocking(move || engines_view(&app)).await.map_err(text)?
}

/// Asks for an engine's executable, over the window that asked; `None` when
/// the user cancelled. The page then chooses it with [`choose_engine`],
/// showing that it is being checked (#73): a slow engine start must not look
/// like a hung window.
#[tauri::command]
pub async fn pick_engine(app: AppHandle, window: WebviewWindow) -> Answer<Option<String>> {
    let strings = &shared(&app).strings;
    let dialog = app
        .dialog()
        .file()
        .set_title(strings.get("settings.engine.pick"))
        .add_filter(strings.get("settings.engine.filter"), &["exe"])
        .set_parent(&window);
    let picked = tauri::async_runtime::spawn_blocking(move || dialog.blocking_pick_file()).await.map_err(text)?;
    let Some(file) = picked else { return Ok(None) };
    let path = file.into_path().map_err(text)?;
    Ok(Some(path.to_string_lossy().into_owned()))
}

/// Asynchronous: in the Store channel it waits on Windows' startup task.
#[tauri::command]
pub async fn set_autostart(app: AppHandle, on: bool) -> Answer<SettingsView> {
    switch_autostart(&app, on)?;
    settings_view(&app)
}

/// Turns starting with Windows on or off, moves the menu's tick with it, and
/// answers where it stands.
pub fn switch_autostart(app: &AppHandle, on: bool) -> Result<State, String> {
    let result = autostart::set(app, on);
    tray::show_autostart(app, autostart::state(app) == State::On);
    result
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
    Ok(shared.pairing()?)
}

#[tauri::command]
pub fn copy_code(app: AppHandle) -> Answer<()> {
    let code = shared(&app).pairing()?.code;
    Ok(app.clipboard().write_text(code).map_err(text)?)
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
/// pairing link. An invalid port is refused with its own message.
#[tauri::command]
pub fn set_port(app: AppHandle, port: String) -> Answer<()> {
    let port = settings::parse_port(&port).ok_or(Failure::new("settings.port.error"))?;
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
