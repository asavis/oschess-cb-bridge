//! Notices: the toasts the bridge shows. The notification plugin sends each
//! one under the app's identifier, the AppUserModelID that the NSIS
//! installer's shortcut registers. Windows shows a packaged app's toast only
//! under an id of its own package, so the Store copy's toasts were dropped
//! without a word and it showed none (#265). In the Store channel a notice
//! therefore goes through the package's own notifier.

use tauri::AppHandle;
use tauri_plugin_notification::NotificationExt;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};
use windows::core::HSTRING;

use crate::toast;

/// Shows a notice with `title` and, when it is not empty, `body`. A notice
/// that could not be shown is logged.
pub(super) fn notify(app: &AppHandle, title: String, body: &str) {
    let shown = if super::channel().is_store() {
        packaged(&title, body)
    } else {
        let mut builder = app.notification().builder().title(title);
        if !body.is_empty() {
            builder = builder.body(body);
        }
        builder.show().map_err(|e| e.to_string())
    };
    if let Err(e) = shown {
        bridge::log!("notice: {e}");
    }
}

/// Shows a toast through the notifier of the calling package's app, which
/// `CreateToastNotifier` without an id gives.
fn packaged(title: &str, body: &str) -> Result<(), String> {
    let text = |e: windows::core::Error| e.to_string();
    let xml = XmlDocument::new().map_err(text)?;
    xml.LoadXml(&HSTRING::from(toast::xml(title, body))).map_err(text)?;
    let notice = ToastNotification::CreateToastNotification(&xml).map_err(text)?;
    ToastNotificationManager::CreateToastNotifier().and_then(|notifier| notifier.Show(&notice)).map_err(text)
}
