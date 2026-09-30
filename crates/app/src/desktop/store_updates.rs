//! Updates in the Store channel (#153). The Store installs a packaged app's
//! update only while the app is closed, and the bridge runs in the tray all
//! the time, so its copy would stay old. The app therefore looks for its own
//! update through the Store API, on the direct channel's schedule, and
//! installs it itself: silently when Windows allows it, and otherwise, when
//! the user asks, through Windows' own dialogs (#232), which start on the UI
//! thread as Microsoft requires; either way Windows closes the bridge and
//! starts it again. [`updates::store_step`] decides which, and
//! [`updates::install_store_update`] keeps the install's order.
//!
//! The Store does not say which version it installs: `StorePackageUpdate`
//! names the package that has an update, not the update. So the notices name
//! no version, and the start after an install says it was updated when it
//! runs another version than the install started from.

use std::sync::Mutex;
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use windows::Services::Store::{StoreContext, StorePackageUpdateState};
use windows::Win32::System::Recovery::{REGISTER_APPLICATION_RESTART_FLAGS, RegisterApplicationRestart};
use windows::Win32::UI::Shell::IInitializeWithWindow;
use windows::core::{AgileReference, Interface, PCWSTR};

use super::server::Shared;
use super::shared;
use super::windows::FLYOUT;
use crate::channel::STORE_PAGE;
use crate::updates::{self, StoreCalls, StoreOutcome, StoreStep};

/// What automatic looks told this run: `Some(false)` that an update waits,
/// `Some(true)` that one is required. A notice is repeated only when it says
/// more than the last one did.
static TOLD: Mutex<Option<bool>> = Mutex::new(None);

/// How often a downloaded update asks again whether it may install.
const READY_POLL: Duration = Duration::from_secs(30);

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

/// Asks Windows to start the bridge again after it closes it for an install.
fn register_restart() -> Result<(), String> {
    unsafe { RegisterApplicationRestart(PCWSTR::null(), REGISTER_APPLICATION_RESTART_FLAGS(0)) }.map_err(text)
}

/// Waits until a Store install would lose no work and Windows would start the
/// bridge again after it (`updates::store_ready`).
fn wait_ready(shared: &Shared) {
    while !updates::store_ready(&shared.view(), super::commands::installing(), super::updater::alive()) {
        std::thread::sleep(READY_POLL);
    }
}

/// Looks for an update through the Store and acts on it; `asked`: the user
/// asked. An install closes the bridge, so this returns only when there was
/// nothing to install, the user declined one of Windows' dialogs, the Store
/// page opened, the user was told, or something failed.
pub fn look_and_install(app: &AppHandle, asked: bool) -> Result<(), String> {
    let context = StoreContext::GetDefault().map_err(text)?;
    // A desktop app's Store context needs a window to own anything it shows,
    // Windows' update dialog included; the flyout always exists, hidden until
    // the tray mark is clicked.
    if let Some(window) = app.get_webview_window(FLYOUT) {
        let hwnd = window.hwnd().map_err(|e| e.to_string())?;
        unsafe { context.cast::<IInitializeWithWindow>().map_err(text)?.Initialize(hwnd).map_err(text)? };
    }
    let found = context.GetAppAndOptionalStorePackageUpdatesAsync().and_then(|op| op.get()).map_err(text)?;
    let count = found.Size().map_err(text)?;
    let mandatory = (0..count).any(|i| found.GetAt(i).and_then(|u| u.Mandatory()).unwrap_or(false));
    let silent = context.CanSilentlyDownloadStorePackageUpdates().map_err(text)?;
    let shared = shared(app);
    let strings = &shared.strings;
    match updates::store_step(count > 0, silent, mandatory, asked) {
        StoreStep::Latest => {
            if asked {
                super::updater::notify_latest(app);
            }
            Ok(())
        }
        StoreStep::RequestInstall => {
            // Microsoft requires the requests, which show Windows' dialogs, to
            // start on the UI thread (0x80070578 otherwise). The updates found
            // reach it through an agile reference, since the collection is not
            // `Send`; the request is waited for here, never on the UI thread.
            let requested = AgileReference::new(&found).map_err(text).and_then(|agile| {
                // `install`: download if needed, then install; otherwise
                // download only. Each asks the user first.
                let request = |install: bool| {
                    let (context, agile) = (context.clone(), agile.clone());
                    let operation = updates::run_on(
                        |job| app.run_on_main_thread(job).map_err(|e| e.to_string()),
                        move || {
                            let found = agile.resolve().map_err(text)?;
                            let started = if install {
                                context.RequestDownloadAndInstallStorePackageUpdatesAsync(&found)
                            } else {
                                context.RequestDownloadStorePackageUpdatesAsync(&found)
                            };
                            started.map_err(text)
                        },
                    )?;
                    let state = operation.get().and_then(|r| r.OverallState()).map_err(text)?;
                    Ok(match state {
                        StorePackageUpdateState::Completed => StoreOutcome::Completed,
                        // Windows reports a dialog the user declined so.
                        StorePackageUpdateState::Canceled => StoreOutcome::Declined,
                        _ => StoreOutcome::Incomplete,
                    })
                };
                let store = StoreCalls { download: || request(false), register_restart, install: || request(true) };
                shared.dir().and_then(|dir| {
                    updates::install_store_update(&store, &dir, env!("CARGO_PKG_VERSION"), || wait_ready(&shared))
                })
            });
            // The Store page stays the way to update when Windows could not
            // ask the user itself.
            requested.or_else(|e| {
                bridge::log!("update: {e}; opening the Store page");
                app.opener().open_url(STORE_PAGE, None::<&str>).map_err(|e| e.to_string())
            })
        }
        StoreStep::Tell { mandatory } => {
            let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
            if told.is_none_or(|was| mandatory && !was) {
                let title = if mandatory {
                    strings.get("toast.update.required.title")
                } else {
                    strings.get("toast.update.waiting.title")
                };
                super::updater::notify(app, title.to_string(), strings.get("toast.update.waiting.body"));
                *told = Some(mandatory);
            }
            Ok(())
        }
        StoreStep::InstallQuietly => {
            let dir = shared.dir()?;
            // Nothing is asked, so nothing can be declined: only `Completed`
            // counts.
            let completed = |state: windows::core::Result<StorePackageUpdateState>| {
                Ok(match state.map_err(text)? {
                    StorePackageUpdateState::Completed => StoreOutcome::Completed,
                    _ => StoreOutcome::Incomplete,
                })
            };
            let store = StoreCalls {
                download: || {
                    completed(
                        context
                            .TrySilentDownloadStorePackageUpdatesAsync(&found)
                            .and_then(|op| op.get())
                            .and_then(|r| r.OverallState()),
                    )
                },
                register_restart,
                install: || {
                    completed(
                        context
                            .TrySilentDownloadAndInstallStorePackageUpdatesAsync(&found)
                            .and_then(|op| op.get())
                            .and_then(|r| r.OverallState()),
                    )
                },
            };
            updates::install_store_update(&store, &dir, env!("CARGO_PKG_VERSION"), || {
                wait_ready(&shared);
                if asked {
                    super::updater::notify(app, strings.get("toast.update.installing.store").to_string(), "");
                }
            })
        }
    }
}
