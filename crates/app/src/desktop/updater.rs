//! Updates on Windows: a look for a new version a minute after the start and
//! every six hours while «Update automatically» is on, and a look on request
//! from the menu or the settings. In the direct channel the updater plugin
//! looks: a new version downloads at once and installs when the bridge is
//! idle, as its installer runs without a window, replaces the app and starts
//! it again. In the Store channel the Store API looks (`store_updates`, #153).
//! Either way the new start says so.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Emitter, Runtime};
use tauri_plugin_updater::UpdaterExt;

use super::notices::notify;
use super::server::Shared;
use super::shared;
use crate::updates::{Phase, Progress};
use crate::{prefs, updates};

const FIRST_LOOK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often a downloaded update asks again whether the bridge is idle.
const IDLE_POLL: Duration = Duration::from_secs(30);

/// The running job and its last result, also available to reopened windows.
static PROGRESS: updates::Monitor = updates::Monitor::new();

pub fn progress() -> Progress {
    PROGRESS.snapshot()
}

pub(super) fn report(app: &AppHandle, phase: Phase, version: Option<String>) {
    let _ = app.emit("update-progress", PROGRESS.report(phase, version));
}

/// When this process started looking for updates, at the app's start.
static STARTED: OnceLock<Instant> = OnceLock::new();

/// How long the app has run, as far as a Store install's restart needs to
/// know (`updates::RESTARTABLE_AFTER`).
pub(super) fn alive() -> Duration {
    STARTED.get_or_init(Instant::now).elapsed()
}

/// The updater plugin, when `config` holds a real key; `None` keeps the
/// updater out, and nothing looks for updates. The plugin gets the key as
/// checked, without surrounding space, never the raw configuration value.
pub fn plugin<R: Runtime>(config: &tauri::Config) -> Option<TauriPlugin<R, tauri_plugin_updater::Config>> {
    let key = updates::public_key(config.plugins.0.get("updater"))?;
    Some(tauri_plugin_updater::Builder::new().pubkey(key).build())
}

/// Whether this build looks for updates: the Store's package always does,
/// through the Store; a direct one when its configuration holds a real key.
pub fn enabled(app: &AppHandle) -> bool {
    super::channel().is_store() || updates::public_key(app.config().plugins.0.get("updater")).is_some()
}

/// Says «updated to X» when this start follows an update, and starts the
/// automatic looks.
pub fn start(app: &AppHandle) {
    STARTED.get_or_init(Instant::now);
    let shared = shared(app);
    if let Some(version) = shared.dir().ok().and_then(|dir| updates::updated(&dir, env!("CARGO_PKG_VERSION"))) {
        bridge::log!("update: this start runs {version}, newly installed");
        report(app, Phase::Updated, Some(version.clone()));
        let title = shared.strings.fill("toast.updated.title", &[("version", &version)]);
        notify(app, title, shared.strings.get("toast.updated.body"));
    }
    if !enabled(app) {
        return;
    }
    let app = app.clone();
    let spawned = std::thread::Builder::new().name("updates".into()).spawn(move || {
        std::thread::sleep(FIRST_LOOK);
        loop {
            if shared.dir().is_ok_and(|dir| prefs::load(&dir).auto_update)
                && let Some(progress) = PROGRESS.begin()
            {
                let _ = app.emit("update-progress", progress);
                look(&app, false);
            }
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        bridge::log!("no update thread: {e}");
    }
}

/// Looks for an update on request, on a thread of its own: the menu and the
/// commands run on the event loop's thread.
pub fn look_now(app: &AppHandle) -> Progress {
    if !enabled(app) {
        return progress();
    }
    let Some(started) = PROGRESS.begin() else { return progress() };
    let _ = app.emit("update-progress", &started);
    let worker_app = app.clone();
    if let Err(e) = std::thread::Builder::new().name("update-now".into()).spawn(move || look(&worker_app, true)) {
        bridge::log!("update: no worker thread: {e}");
        report(app, Phase::Failed, None);
    }
    progress()
}

/// Completes the job already reserved by its caller. Every exit records a
/// terminal result before another request can reserve a job.
fn look(app: &AppHandle, asked: bool) {
    let (progress, error) = PROGRESS.finish(|| look_and_install(app, asked));
    let _ = app.emit("update-progress", progress);
    if let Some(e) = error {
        bridge::log!("update: {e}");
        if asked {
            let strings = &shared(app).strings;
            notify(app, strings.get("toast.update.failed.title").to_string(), strings.get("toast.update.failed.body"));
        }
    }
}

/// On success the installer runs and this process exits, so this returns
/// only when there was nothing to install or something failed. [`look`] logs
/// the error as it is, so no error of this crate's own names a path.
fn look_and_install(app: &AppHandle, asked: bool) -> Result<Phase, String> {
    if super::channel().is_store() {
        return super::store_updates::look_and_install(app, asked);
    }
    let shared = shared(app);
    let strings = &shared.strings;
    let found = tauri::async_runtime::block_on(async { app.updater()?.check().await }).map_err(|e| e.to_string())?;
    let Some(update) = found else {
        if asked {
            notify_latest(app);
        }
        return Ok(Phase::Current);
    };
    // The download checks the signature; an installer the key did not sign
    // never runs.
    report(app, Phase::Downloading, Some(update.version.clone()));
    let bytes = tauri::async_runtime::block_on(update.download(|_, _| {}, || {})).map_err(|e| e.to_string())?;
    if !updates::idle(&shared.view(), super::commands::installing()) {
        report(app, Phase::WaitingIdle, Some(update.version.clone()));
    }
    wait_idle(&shared);
    report(app, Phase::Installing, Some(update.version.clone()));
    if asked {
        notify(app, strings.fill("toast.update.installing", &[("version", &update.version)]), "");
    }
    let dir = shared.dir()?;
    updates::note(&dir, &update.version)?;
    let installed = update.install(bytes);
    updates::forget(&dir);
    installed.map(|()| Phase::RestartRequired).map_err(|e| e.to_string())
}

/// Waits until an install would lose no work (`updates::idle`).
pub(super) fn wait_idle(shared: &Shared) {
    while !updates::idle(&shared.view(), super::commands::installing()) {
        std::thread::sleep(IDLE_POLL);
    }
}

/// Says this version is the newest, after a look the user asked for.
pub(super) fn notify_latest(app: &AppHandle) {
    let strings = &shared(app).strings;
    let title = strings.get("toast.update.latest.title").to_string();
    notify(app, title, &strings.fill("toast.update.latest.body", &[("version", env!("CARGO_PKG_VERSION"))]));
}

#[cfg(test)]
mod tests {
    /// The plugin accepts the `updater` section that ships: one https
    /// endpoint, a version bound into the signature, and an installer without
    /// a window.
    #[test]
    fn the_plugin_accepts_the_shipped_configuration() {
        let config: serde_json::Value = serde_json::from_str(include_str!("../../tauri.conf.json")).unwrap();
        let updater: tauri_plugin_updater::Config =
            serde_json::from_value(config["plugins"]["updater"].clone()).expect("the plugin reads the section");
        assert_eq!(updater.endpoints.iter().map(|u| u.scheme()).collect::<Vec<_>>(), ["https"]);
        assert!(updater.require_signed_version);
        assert!(!updater.allow_downgrades);
        assert_eq!(updater.windows.map(|w| w.install_mode.to_string()).as_deref(), Some("quiet"));
    }
}
