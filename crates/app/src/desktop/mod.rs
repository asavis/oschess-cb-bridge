//! The Tauri app: the tray mark and its menu, the status flyout, the settings
//! and first-run windows, and the commands the windows call. Windows only.

mod commands;
mod server;
mod system;
mod tray;
mod updater;
mod windows;

use std::sync::Arc;
use std::time::Duration;

use tauri::{Emitter, Manager, RunEvent};
use tauri_plugin_autostart::MacosLauncher;
use tauri_plugin_notification::NotificationExt;

use crate::i18n::{Lang, Strings};
use crate::status::{Problem, View};
use server::Shared;

/// The argument the Run key starts the app with, so a start at sign-in can be
/// told from one by hand.
const AUTOSTART_ARG: &str = "--autostart";

pub fn run() {
    let strings = Strings::new(Lang::from_langid(system::display_language()));
    let context = tauri::generate_context!();
    let mut builder = tauri::Builder::default()
        // First: a second start hands over to this one and exits.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // A second start before this one finished starting has nothing to open yet.
            if app.try_state::<Arc<Shared>>().is_some() {
                commands::open_oschess_now(app);
            }
        }))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![AUTOSTART_ARG])))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init());
    // Only with a real key in tauri.conf.json: the placeholder keeps updates off.
    if let Some(updater) = updater::plugin(context.config()) {
        builder = builder.plugin(updater);
    }
    let app = builder
        .invoke_handler(tauri::generate_handler![
            commands::view,
            commands::open_oschess,
            commands::open_settings,
            commands::hide_flyout,
            commands::fit_flyout,
            commands::settings,
            commands::add_folder,
            commands::remove_database,
            commands::set_autostart,
            commands::set_auto_update,
            commands::pairing_code,
            commands::copy_code,
            commands::new_code,
            commands::set_port,
            commands::open_pairing,
            commands::check_updates,
        ])
        // The plugins set up first, so a second start has handed over and
        // exited before this one touches the port.
        .setup(move |app| {
            let started = Shared::start(strings);
            let shared = Arc::new(started.shared);
            app.manage(shared.clone());
            tray::create(app.handle())?;
            windows::create_flyout(app.handle());
            if started.first_run {
                windows::open_first_run(app.handle());
            }
            if started.pair {
                commands::open_pairing_now(app.handle());
            }
            watch(app.handle().clone(), shared);
            updater::start(app.handle());
            Ok(())
        })
        .on_window_event(windows::on_event)
        .build(context);
    match app {
        Ok(app) => app.run(|_, event| {
            // Closing the last window keeps the bridge running in the tray;
            // only «Quit» ends it.
            if let RunEvent::ExitRequested { api, code: None, .. } = event {
                api.prevent_exit();
            }
        }),
        Err(e) => {
            eprintln!("oschess bridge: {e}");
            std::process::exit(1);
        }
    }
}

/// Follows the bridge once a second: redraws the tray mark when the state,
/// the taskbar theme or the display scale changes, sends the windows the new
/// view, and tells the user once when a problem appears.
fn watch(app: tauri::AppHandle, shared: Arc<Shared>) {
    let spawned = std::thread::Builder::new().name("bridge-status".into()).spawn(move || {
        let mut last: Option<View> = None;
        let mut drawn = String::new();
        loop {
            let view = shared.current_view();
            let theme = system::taskbar_theme();
            let scale = app.primary_monitor().ok().flatten().map_or(1.0, |m| m.scale_factor());
            let key = tray::update(&app, &shared.strings, &view, theme, scale, &drawn);
            drawn = key;
            if last.as_ref() != Some(&view) {
                if view.problem.is_some() && last.as_ref().is_none_or(|l| l.problem.is_none()) {
                    notify_problem(&app, &shared.strings, &view);
                }
                let _ = app.emit("view", &view);
                shared.set_view(view.clone());
                last = Some(view);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
    if let Err(e) = spawned {
        eprintln!("oschess bridge: no status thread: {e}");
    }
}

fn notify_problem(app: &tauri::AppHandle, strings: &Strings, view: &View) {
    let (title, body) = match &view.problem {
        Some(Problem::PortBusy { port }) => (
            strings.fill("toast.portBusy.title", &[("port", &port.to_string())]),
            strings.get("toast.portBusy.body").to_string(),
        ),
        _ => (strings.get("toast.stopped.title").to_string(), strings.get("toast.stopped.body").to_string()),
    };
    let _ = app.notification().builder().title(title).body(body).show();
}

/// The shared state, as the commands receive it.
pub(crate) type SharedState<'a> = tauri::State<'a, Arc<Shared>>;

/// The shared state from anywhere that holds the app.
pub(crate) fn shared(app: &tauri::AppHandle) -> Arc<Shared> {
    app.state::<Arc<Shared>>().inner().clone()
}
