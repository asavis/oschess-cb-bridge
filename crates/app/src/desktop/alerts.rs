//! Native messages for when one of the app's own windows fails (#332), and
//! for a report sent by hand (#333). A message box needs neither WebView2 nor
//! the event loop, so it still works when one of them is what failed.
//! [`warn`] and [`inform`] run on a thread of their own, so the caller never
//! waits for the user to close them; [`ask`] waits for the answer.

use windows_sys::Win32::UI::WindowsAndMessaging::{
    IDOK, MB_ICONINFORMATION, MB_ICONQUESTION, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MB_SETFOREGROUND, MB_TOPMOST,
    MESSAGEBOX_STYLE, MessageBoxW,
};

/// Shows `text` under `title` with a warning sign, above the other windows.
pub fn warn(title: &str, text: &str) {
    later(title, text, MB_ICONWARNING);
}

/// Shows `text` under `title` with an information sign.
pub fn inform(title: &str, text: &str) {
    later(title, text, MB_ICONINFORMATION);
}

/// Asks `text` under `title`, and answers whether the user chose OK.
pub fn ask(title: &str, text: &str) -> bool {
    show(&wide(title), &wide(text), MB_OKCANCEL | MB_ICONQUESTION) == IDOK
}

fn later(title: &str, text: &str, icon: MESSAGEBOX_STYLE) {
    let (title, text) = (wide(title), wide(text));
    let spawned = std::thread::Builder::new().name("message".into()).spawn(move || {
        show(&title, &text, MB_OK | icon);
    });
    if let Err(e) = spawned {
        bridge::fault!("no message: {e}");
    }
}

fn show(title: &[u16], text: &[u16], style: MESSAGEBOX_STYLE) -> i32 {
    // SAFETY: both strings are NUL-terminated UTF-16 that outlive the call,
    // and the message has no owner window.
    unsafe { MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), style | MB_SETFOREGROUND | MB_TOPMOST) }
}

/// `text` as a NUL-terminated UTF-16 string.
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
