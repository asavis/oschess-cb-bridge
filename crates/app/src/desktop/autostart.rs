//! «Start with Windows» in both channels (#112). The NSIS installation writes
//! the per-user Run value through tauri-plugin-autostart. Inside a package that
//! value would land in the package's own registry, which Windows never reads
//! at sign-in, so the Store channel enables the startup task its manifest
//! declares instead.

use tauri::AppHandle;
use tauri_plugin_autostart::ManagerExt;

use crate::channel::STARTUP_TASK;

/// Whether the app starts with Windows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    On,
    Off,
    /// The user turned the startup task off in Windows' Startup apps
    /// settings, or a policy did: only there can it be turned on again.
    Blocked,
}

pub fn state(app: &AppHandle) -> State {
    if super::channel().is_store() {
        return off_thread(task_state).unwrap_or(State::Off);
    }
    if app.autolaunch().is_enabled().unwrap_or(false) { State::On } else { State::Off }
}

/// Turns starting with Windows on or off and answers where it stands.
pub fn set(app: &AppHandle, on: bool) -> Result<State, String> {
    if super::channel().is_store() {
        return off_thread(move || set_task(on));
    }
    let launcher = app.autolaunch();
    let result = if on { launcher.enable() } else { launcher.disable() };
    result.map_err(|e| e.to_string())?;
    Ok(state(app))
}

/// Runs a WinRT call on its own thread: the caller may be the event loop's
/// thread, which must not wait on an asynchronous operation.
fn off_thread<T: Send + 'static>(call: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    std::thread::Builder::new()
        .name("startup-task".into())
        .spawn(call)
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "the startup task call panicked".to_string())?
}

fn task() -> Result<windows::ApplicationModel::StartupTask, String> {
    let id = windows::core::HSTRING::from(STARTUP_TASK);
    windows::ApplicationModel::StartupTask::GetAsync(&id).and_then(|op| op.get()).map_err(|e| e.to_string())
}

fn task_state() -> Result<State, String> {
    Ok(of(task()?.State().map_err(|e| e.to_string())?))
}

fn set_task(on: bool) -> Result<State, String> {
    let task = task()?;
    if on {
        // Windows enables a disabled task without asking; one the user turned
        // off stays off and answers so.
        return Ok(of(task.RequestEnableAsync().and_then(|op| op.get()).map_err(|e| e.to_string())?));
    }
    task.Disable().map_err(|e| e.to_string())?;
    Ok(of(task.State().map_err(|e| e.to_string())?))
}

fn of(state: windows::ApplicationModel::StartupTaskState) -> State {
    use windows::ApplicationModel::StartupTaskState as S;
    match state {
        S::Enabled | S::EnabledByPolicy => State::On,
        S::DisabledByUser | S::DisabledByPolicy => State::Blocked,
        _ => State::Off,
    }
}
