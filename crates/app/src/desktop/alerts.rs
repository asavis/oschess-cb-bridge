//! Native messages for when one of the app's own windows fails (#332). A
//! message box needs neither WebView2 nor the event loop, so it still works
//! when one of them is what failed. Each runs on a thread of its own, so the
//! caller never waits for the user to close it.

use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MessageBoxW};

/// Shows `text` under `title` with a warning sign, above the other windows.
pub fn warn(title: &str, text: &str) {
    let (title, text) = (wide(title), wide(text));
    let spawned = std::thread::Builder::new().name("message".into()).spawn(move || {
        // SAFETY: both strings are NUL-terminated UTF-16 that outlive the
        // call, and the message has no owner window.
        unsafe {
            MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                MB_OK | MB_ICONWARNING | MB_SETFOREGROUND | MB_TOPMOST,
            )
        };
    });
    if let Err(e) = spawned {
        bridge::log!("no message: {e}");
    }
}

/// `text` as a NUL-terminated UTF-16 string.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
