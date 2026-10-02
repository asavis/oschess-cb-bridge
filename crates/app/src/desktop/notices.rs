//! Notices: the toasts the bridge shows (#265). Each goes to Windows'
//! notifier here, so that one Windows refuses is logged. The notification
//! plugin dropped that error, and so hid that the Store copy showed none:
//! it sent every toast under the app's identifier, the AppUserModelID the
//! NSIS installer's shortcut registers, and Windows shows a packaged app's
//! toast only under an id of its own package.

use tauri::AppHandle;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

use crate::toast;

/// Shows a notice with `title` and, when it is not empty, `body`. A notice
/// that could not be shown is logged.
pub(super) fn notify(app: &AppHandle, title: String, body: &str) {
    if let Err(e) = show(app, &title, body) {
        bridge::log!("notice: {e}");
    }
}

/// Shows a toast: in the Store channel through the notifier of the calling
/// package's app, which `CreateToastNotifier` without an id gives; in the
/// direct channel under the id `toast::direct_app_id` names.
fn show(app: &AppHandle, title: &str, body: &str) -> Result<(), String> {
    let text = |e: windows::core::Error| e.to_string();
    let xml = XmlDocument::new().map_err(text)?;
    xml.LoadXml(&HSTRING::from(toast::xml(title, body))).map_err(text)?;
    let notice = ToastNotification::CreateToastNotification(&xml).map_err(text)?;
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
    notifier.and_then(|notifier| notifier.Show(&notice)).map_err(text)
}
