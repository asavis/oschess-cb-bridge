//! Updates on Windows: the updater plugin, a look for a new version a minute
//! after the start and every six hours while «Update automatically» is on, and
//! a look on request from the menu or the settings. A new version downloads at
//! once and installs when the bridge is idle: its installer runs without a
//! window, replaces the app and starts it again, and the new start says so.

use std::time::Duration;

use tauri::plugin::TauriPlugin;
use tauri::{AppHandle, Runtime};
use tauri_plugin_notification::NotificationExt;
use tauri_plugin_updater::UpdaterExt;

use super::shared;
use crate::{prefs, updates};

const FIRST_LOOK: Duration = Duration::from_secs(60);
const EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How often a downloaded update asks again whether the bridge is idle.
const IDLE_POLL: Duration = Duration::from_secs(30);

/// One look or install at a time; a look on request waits for a running one.
static GATE: updates::Gate = updates::Gate::new();

/// The updater plugin, when `config` holds a real key; `None` keeps the
/// updater out, and nothing looks for updates. The plugin gets the key as
/// checked, without surrounding space, never the raw configuration value.
pub fn plugin<R: Runtime>(config: &tauri::Config) -> Option<TauriPlugin<R, tauri_plugin_updater::Config>> {
    let key = updates::public_key(config.plugins.0.get("updater"))?;
    Some(tauri_plugin_updater::Builder::new().pubkey(key).build())
}

/// Whether this build looks for updates: its configuration holds a real key.
pub fn enabled(app: &AppHandle) -> bool {
    updates::public_key(app.config().plugins.0.get("updater")).is_some()
}

/// Says «updated to X» when this start follows an update, and starts the
/// automatic looks.
pub fn start(app: &AppHandle) {
    let shared = shared(app);
    if let Some(version) = shared.dir().ok().and_then(|dir| updates::updated(&dir, env!("CARGO_PKG_VERSION"))) {
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
            if shared.dir().is_ok_and(|dir| prefs::load(&dir).auto_update) {
                look(&app, false);
            }
            std::thread::sleep(EVERY);
        }
    });
    if let Err(e) = spawned {
        eprintln!("oschess bridge: no update thread: {e}");
    }
}

/// Looks for an update on request, on a thread of its own: the menu and the
/// commands run on the event loop's thread.
pub fn look_now(app: &AppHandle) {
    if !enabled(app) {
        return;
    }
    let app = app.clone();
    let _ = std::thread::Builder::new().name("update-now".into()).spawn(move || look(&app, true));
}

/// Looks for a newer version and installs it. `asked`: the user asked, so the
/// outcome is told whatever it is, after any look already running; otherwise
/// only the new start speaks, and a look already running makes this one skip.
fn look(app: &AppHandle, asked: bool) {
    let Some(running) = GATE.enter(asked) else { return };
    let outcome = look_and_install(app, asked);
    drop(running);
    if let Err(e) = outcome {
        eprintln!("oschess bridge: update: {e}");
        if asked {
            let strings = &shared(app).strings;
            notify(app, strings.get("toast.update.failed.title").to_string(), strings.get("toast.update.failed.body"));
        }
    }
}

/// On success the installer runs and this process exits, so this returns
/// only when there was nothing to install or something failed.
fn look_and_install(app: &AppHandle, asked: bool) -> Result<(), String> {
    let shared = shared(app);
    let strings = &shared.strings;
    let found = tauri::async_runtime::block_on(async { app.updater()?.check().await }).map_err(|e| e.to_string())?;
    let Some(update) = found else {
        if asked {
            let title = strings.get("toast.update.latest.title").to_string();
            notify(app, title, &strings.fill("toast.update.latest.body", &[("version", env!("CARGO_PKG_VERSION"))]));
        }
        return Ok(());
    };
    // The download checks the signature; an installer the key did not sign
    // never runs.
    let bytes = tauri::async_runtime::block_on(update.download(|_, _| {}, || {})).map_err(|e| e.to_string())?;
    while !updates::idle(&shared.view(), super::commands::installing()) {
        std::thread::sleep(IDLE_POLL);
    }
    if asked {
        notify(app, strings.fill("toast.update.installing", &[("version", &update.version)]), "");
    }
    let dir = shared.dir()?;
    updates::note(&dir, &update.version)?;
    let installed = update.install(bytes);
    updates::forget(&dir);
    installed.map_err(|e| e.to_string())
}

fn notify(app: &AppHandle, title: String, body: &str) {
    let mut toast = app.notification().builder().title(title);
    if !body.is_empty() {
        toast = toast.body(body);
    }
    let _ = toast.show();
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
