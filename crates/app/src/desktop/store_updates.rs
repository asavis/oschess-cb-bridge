//! Updates in the Store channel (#153). The Store installs a packaged app's
//! update only while the app is closed, and the bridge runs in the tray all
//! the time, so its copy would stay old. The app therefore looks for its own
//! update through the Store API, on the direct channel's schedule, and
//! installs it itself: silently when Windows allows it, as Windows closes the
//! bridge and starts it again, and otherwise from the bridge's page in the
//! Store. [`updates::store_step`] decides which.

use std::sync::Mutex;

use tauri::{AppHandle, Manager};
use tauri_plugin_opener::OpenerExt;
use windows::Services::Store::{StoreContext, StorePackageUpdateState};
use windows::Win32::System::Recovery::{REGISTER_APPLICATION_RESTART_FLAGS, RegisterApplicationRestart};
use windows::Win32::UI::Shell::IInitializeWithWindow;
use windows::core::{Interface, PCWSTR};

use super::shared;
use super::windows::FLYOUT;
use crate::channel::STORE_PAGE;
use crate::updates::{self, StoreStep};

/// The version an automatic look last told about, so that each is told once.
static TOLD: Mutex<Option<String>> = Mutex::new(None);

fn text(e: windows::core::Error) -> String {
    e.to_string()
}

/// Looks for an update through the Store and acts on it; `asked`: the user
/// asked. A silent install closes the bridge, so this returns only when
/// there was nothing to install, the Store page opened, the user was told,
/// or something failed.
pub fn look_and_install(app: &AppHandle, asked: bool) -> Result<(), String> {
    let context = StoreContext::GetDefault().map_err(text)?;
    // A desktop app's Store context needs a window to own anything it shows;
    // the flyout always exists, hidden until the tray mark is clicked.
    if let Some(window) = app.get_webview_window(FLYOUT) {
        let hwnd = window.hwnd().map_err(|e| e.to_string())?;
        unsafe { context.cast::<IInitializeWithWindow>().map_err(text)?.Initialize(hwnd).map_err(text)? };
    }
    let found = context.GetAppAndOptionalStorePackageUpdatesAsync().and_then(|op| op.get()).map_err(text)?;
    let count = found.Size().map_err(text)?;
    let mandatory = (0..count).any(|i| found.GetAt(i).and_then(|u| u.Mandatory()).unwrap_or(false));
    let version = match count {
        0 => String::new(),
        _ => {
            let v = found.GetAt(0).and_then(|u| u.Package()).and_then(|p| p.Id()).and_then(|id| id.Version());
            let v = v.map_err(text)?;
            format!("{}.{}.{}", v.Major, v.Minor, v.Build)
        }
    };
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
        StoreStep::OpenStore => app.opener().open_url(STORE_PAGE, None::<&str>).map_err(|e| e.to_string()),
        StoreStep::Tell { mandatory } => {
            let mut told = TOLD.lock().unwrap_or_else(|e| e.into_inner());
            if told.as_deref() != Some(version.as_str()) {
                let title = if mandatory { "toast.update.required.title" } else { "toast.update.waiting.title" };
                super::updater::notify(
                    app,
                    strings.fill(title, &[("version", &version)]),
                    strings.get("toast.update.waiting.body"),
                );
                *told = Some(version);
            }
            Ok(())
        }
        StoreStep::InstallQuietly => {
            super::updater::wait_idle(&shared);
            if asked {
                super::updater::notify(app, strings.fill("toast.update.installing", &[("version", &version)]), "");
            }
            let dir = shared.dir()?;
            updates::note(&dir, &version)?;
            // Windows closes the bridge for the install and, with this, starts
            // it again once the new version is in place.
            unsafe { RegisterApplicationRestart(PCWSTR::null(), REGISTER_APPLICATION_RESTART_FLAGS(0)) }
                .map_err(text)?;
            let result = context.TrySilentDownloadAndInstallStorePackageUpdatesAsync(&found).and_then(|op| op.get());
            // Still running: the new version did not take this one's place.
            updates::forget(&dir);
            match result.and_then(|r| r.OverallState()).map_err(text)? {
                StorePackageUpdateState::Completed => Ok(()),
                state => Err(format!("the Store install ended in state {}", state.0)),
            }
        }
    }
}
