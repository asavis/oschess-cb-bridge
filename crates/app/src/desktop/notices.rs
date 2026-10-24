//! Notices: the toasts the bridge shows (#265). Each goes to Windows'
//! notifier here, so that one Windows refuses is logged. The notification
//! plugin dropped that error, and so hid that the Store copy showed none:
//! it sent every toast under the app's identifier, the AppUserModelID the
//! NSIS installer's shortcut registers, and Windows shows a packaged app's
//! toast only under an id of its own package.

use std::sync::Mutex;

use bridge::sync::lock;
use tauri::AppHandle;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Foundation::TypedEventHandler;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::{HSTRING, IInspectable};

use crate::toast;

/// What a click on a notice does.
type Click = TypedEventHandler<ToastNotification, IInspectable>;

/// The last notice shown whose click opens the settings (#344), held until
/// the next one: Windows tells of the click through the notice the app holds.
static CLICKABLE: Mutex<Option<ToastNotification>> = Mutex::new(None);

/// Shows a notice with `title` and, when it is not empty, `body`. A notice
/// that could not be shown is logged.
pub(super) fn notify(app: &AppHandle, title: String, body: &str) {
    if let Err(e) = show(app, &title, body, None) {
        bridge::fault!("notice: {e}");
    }
}

/// Shows a notice as [`notify`] does, and opens the settings window at
/// `section` when the user clicks it while the bridge runs (#344).
pub(super) fn notify_opening(app: &AppHandle, title: String, body: &str, section: &'static str) {
    let opener = app.clone();
    let click = Click::new(move |_, _| {
        super::windows::open_settings(&opener, section);
        Ok(())
    });
    if let Err(e) = show(app, &title, body, Some(click)) {
        bridge::fault!("notice: {e}");
    }
}

/// Shows a toast: in the Store channel through the notifier of the calling
/// package's app, which `CreateToastNotifier` without an id gives; in the
/// direct channel under the id `toast::direct_app_id` names. A toast with a
/// `click` is held as [`CLICKABLE`].
fn show(app: &AppHandle, title: &str, body: &str, click: Option<Click>) -> Result<(), String> {
    let text = |e: windows::core::Error| e.to_string();
    let xml = XmlDocument::new().map_err(text)?;
    xml.LoadXml(&HSTRING::from(toast::xml(title, body))).map_err(text)?;
    let notice = ToastNotification::CreateToastNotification(&xml).map_err(text)?;
    let clickable = click.is_some();
    if let Some(click) = click {
        notice.Activated(&click).map_err(text)?;
    }
    let notifier = if super::channel().is_store() {
        ToastNotificationManager::CreateToastNotifier()
    } else {
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let dir = exe.parent().unwrap_or(&exe);
        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(toast::direct_app_id(
            &app.config().identifier,
            dir,
        )))
    };
    notifier.and_then(|notifier| notifier.Show(&notice)).map_err(text)?;
    if clickable {
        *lock(&CLICKABLE) = Some(notice);
    }
    Ok(())
}
